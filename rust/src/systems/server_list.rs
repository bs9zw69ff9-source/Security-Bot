//! `/servers`: DM the bot owner an invite to every server the bot is in.
//!
//! Reuses a permanent invite where the server already has one, so running this
//! repeatedly doesn't litter every server's invite list, and only falls back to
//! creating one when there's nothing suitable to reuse.

use serenity::all::*;
use serenity::builder::{CreateEmbed, CreateInvite, CreateMessage};
use std::collections::HashSet;
use std::time::Duration;

use crate::colors;

/// How long one server's lookup (its owner and an invite) may take. Serenity
/// puts no timeout on a request and sleeps through rate limits, invite
/// creation's included, so before this one slow or rate-limited server held up
/// the whole list, and the command never answered.
const PER_SERVER: Duration = Duration::from_secs(20);
/// Servers looked up at once.
const CONCURRENCY: usize = 6;
/// The whole list has to be done well inside the 15 minutes Discord accepts a
/// reply for. Servers still being looked up then are listed as timed out.
const TOTAL: Duration = Duration::from_secs(8 * 60);
/// A single DM page gets this long to send.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// How many channels to try before giving up on a server. A server that says no
/// to the first few is going to say no to the rest, and every attempt is another
/// request against the rate limit.
const CHANNEL_ATTEMPTS: usize = 5;

/// Discord's embed description limit is 4096; leave room for the header.
const PAGE_LIMIT: usize = 3800;

pub struct Entry {
    pub id: GuildId,
    pub name: String,
    pub members: Option<u64>,
    /// The owner's tag (e.g. `name`), or their id when the lookup failed.
    pub owner: String,
    pub owner_id: Option<UserId>,
    /// When the server was created, from its id. Unix seconds.
    pub created: i64,
    /// The invite, or why there isn't one.
    pub invite: Result<String, String>,
}

/// Walk every server the bot is in and work out an invite for each, a few at
/// a time, each within PER_SERVER and all within TOTAL. A server that runs out
/// of time is still listed, from the cache, with the invite marked timed out.
pub async fn collect(ctx: &Context) -> Vec<Entry> {
    let mut out = bounded(
        ctx.cache.guilds(),
        PER_SERVER,
        TOTAL,
        |gid| entry_for(ctx, gid),
        |gid, why| from_cache(ctx, gid, why),
    )
    .await;
    // Biggest first, which is usually the order you care about.
    out.sort_by(|a, b| b.members.unwrap_or(0).cmp(&a.members.unwrap_or(0)));
    out
}

const SLOW: &str = "timed out (Discord was slow or rate-limiting)";
const OUT_OF_TIME: &str = "timed out (ran out of time for the whole list)";

/// Run `lookup` over `items`, CONCURRENCY at a time, each within `per_item`
/// and all of them within `total`. Every item comes back exactly once: one that
/// ran out of time comes back as `timed_out(item, why)`.
async fn bounded<I, T, F, Fut>(
    items: Vec<I>,
    per_item: Duration,
    total: Duration,
    lookup: F,
    timed_out: impl Fn(I, &str) -> T,
) -> Vec<T>
where
    I: Copy + Eq + std::hash::Hash,
    F: Fn(I) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    use futures::StreamExt;

    let deadline = tokio::time::Instant::now() + total;
    let mut pending: HashSet<I> = items.iter().copied().collect();
    let mut out = Vec::with_capacity(items.len());
    {
        let (lookup, timed_out) = (&lookup, &timed_out);
        let mut lookups = futures::stream::iter(items)
            .map(|item| async move {
                let found = tokio::time::timeout(per_item, lookup(item)).await;
                (item, found.unwrap_or_else(|_| timed_out(item, SLOW)))
            })
            .buffer_unordered(CONCURRENCY);
        while let Ok(Some((item, found))) = tokio::time::timeout_at(deadline, lookups.next()).await {
            pending.remove(&item);
            out.push(found);
        }
    }
    out.extend(pending.into_iter().map(|item| timed_out(item, OUT_OF_TIME)));
    out
}

/// One server, looked up in full.
async fn entry_for(ctx: &Context, gid: GuildId) -> Entry {
    // Cached name first, since it costs nothing. Falling back to HTTP keeps a
    // server that hasn't finished arriving in the cache from showing up as a
    // bare id.
    let cached = ctx.cache.guild(gid).map(|g| (g.name.clone(), g.member_count, g.owner_id));
    let (name, members, owner_id) = match cached {
        Some((n, c, o)) => (n, Some(c), Some(o)),
        None => match gid.to_partial_guild(&ctx.http).await {
            Ok(g) => (g.name.clone(), g.approximate_member_count, Some(g.owner_id)),
            Err(_) => (format!("Unknown server ({gid})"), None, None),
        },
    };
    // The owner's tag, best-effort: a readable name beats a bare id, but a
    // failed lookup still leaves the id, which is all the owner really needs.
    let owner = match owner_id {
        Some(oid) => match cached_tag(ctx, oid) {
            Some(tag) => tag,
            None => match oid.to_user(&ctx.http).await {
                Ok(u) => u.tag(),
                Err(_) => oid.to_string(),
            },
        },
        None => "unknown".to_string(),
    };
    Entry {
        id: gid,
        name,
        members,
        owner,
        owner_id,
        created: gid.created_at().unix_timestamp(),
        invite: invite_for(ctx, gid).await,
    }
}

/// A server listed from the cache alone, with no invite, for one whose lookup
/// ran out of time.
fn from_cache(ctx: &Context, gid: GuildId, why: &str) -> Entry {
    let cached = ctx.cache.guild(gid).map(|g| (g.name.clone(), g.member_count, g.owner_id));
    let (name, members, owner_id) = match cached {
        Some((n, c, o)) => (n, Some(c), Some(o)),
        None => (format!("Unknown server ({gid})"), None, None),
    };
    let owner = owner_id
        .map(|o| cached_tag(ctx, o).unwrap_or_else(|| o.to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    Entry {
        id: gid,
        name,
        members,
        owner,
        owner_id,
        created: gid.created_at().unix_timestamp(),
        invite: Err(why.to_string()),
    }
}

fn cached_tag(ctx: &Context, user: UserId) -> Option<String> {
    ctx.cache.user(user).map(|u| u.tag())
}

/// An existing permanent invite if there is one, otherwise a fresh one.
async fn invite_for(ctx: &Context, gid: GuildId) -> Result<String, String> {
    // Listing needs Manage Server, which the bot often won't have. That's fine:
    // it just means we skip straight to making one.
    if let Ok(existing) = gid.invites(&ctx.http).await {
        if let Some(inv) = existing.iter().find(|i| i.max_age == 0 && i.max_uses == 0 && !i.temporary) {
            return Ok(inv.url());
        }
    }

    let channels = match gid.channels(&ctx.http).await {
        Ok(c) => c,
        Err(e) => return Err(format!("couldn't read the channel list ({})", short_error(&e))),
    };
    let mut candidates: Vec<&GuildChannel> = channels
        .values()
        .filter(|c| matches!(c.kind, ChannelType::Text | ChannelType::News | ChannelType::Voice))
        .collect();
    // Text channels first, then by position, so the invite points somewhere
    // sensible rather than at whichever channel happened to sort first.
    candidates.sort_by_key(|c| (c.kind != ChannelType::Text, c.position, c.id));

    if candidates.is_empty() {
        return Err("no channel I could make an invite in".to_string());
    }
    // &ctx.http rather than ctx: the builder's cache check can only say no on
    // stale data, and Discord is the authority on whether this is allowed.
    let mut last = String::new();
    for c in candidates.iter().take(CHANNEL_ATTEMPTS) {
        let builder = CreateInvite::new().max_age(0).max_uses(0).unique(false);
        match c.id.create_invite(&ctx.http, builder).await {
            Ok(inv) => return Ok(inv.url()),
            Err(e) => last = short_error(&e),
        }
    }
    Err(if last.is_empty() { "couldn't make an invite".to_string() } else { last })
}

/// Discord's error text is long and repeats itself. Keep the useful part.
fn short_error(e: &serenity::Error) -> String {
    let s = e.to_string();
    if s.contains("Missing Access") || s.contains("50001") {
        return "no access".to_string();
    }
    if s.contains("Missing Permissions") || s.contains("50013") {
        return "missing the Create Invite permission".to_string();
    }
    s.chars().take(80).collect()
}

/// Split the list into embed-sized pages. Pure, so the paging is testable.
pub fn build_pages(entries: &[Entry]) -> Vec<String> {
    let mut pages: Vec<String> = Vec::new();
    let mut current = String::new();
    for e in entries {
        let members = match e.members {
            Some(n) => format!("{} members", thousands(n)),
            None => "unknown members".to_string(),
        };
        let owner = match e.owner_id {
            Some(oid) => format!("{} (<@{}>)", e.owner, oid),
            None => e.owner.clone(),
        };
        let invite = match &e.invite {
            Ok(url) => url.clone(),
            Err(why) => format!("No invite: {why}"),
        };
        let line = format!(
            "**{}**\n🆔 `{}`\n👑 Owner: {}\n👥 {} · 📅 Created <t:{}:D>\n🔗 {}\n\n",
            e.name, e.id, owner, members, e.created, invite
        );
        if !current.is_empty() && current.chars().count() + line.chars().count() > PAGE_LIMIT {
            pages.push(std::mem::take(&mut current));
        }
        current.push_str(&line);
    }
    if !current.is_empty() {
        pages.push(current);
    }
    pages
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Collect the servers and DM them. Returns a line to show the caller.
pub async fn dm_server_list(ctx: &Context, to: UserId) -> String {
    let entries = collect(ctx).await;
    if entries.is_empty() {
        return "I'm not in any servers yet, so there was nothing to send.".to_string();
    }
    let total = entries.len();
    let with_invite = entries.iter().filter(|e| e.invite.is_ok()).count();

    let Ok(user) = to.to_user(&ctx.http).await else {
        return format!("I found {total} servers but couldn't look up <@{to}> to send them to.");
    };
    let Ok(dm) = user.create_dm_channel(&ctx.http).await else {
        return format!("I found {total} servers but couldn't open a DM with <@{to}>. Their DMs are probably closed.");
    };

    let pages = build_pages(&entries);
    let page_count = pages.len();
    for (idx, page) in pages.into_iter().enumerate() {
        let title = if page_count > 1 {
            format!("🌐  SERVERS I'M IN • {} of {page_count}", idx + 1)
        } else {
            "🌐  SERVERS I'M IN".to_string()
        };
        let mut e = CreateEmbed::new().color(colors::INFO).title(title).description(page);
        if idx + 1 == page_count {
            e = e.footer(crate::common::theme::footer(&format!(
                "{total} servers, {with_invite} with an invite"
            )));
        }
        let sent = tokio::time::timeout(SEND_TIMEOUT, dm.send_message(&ctx.http, CreateMessage::new().embed(e))).await;
        if !matches!(sent, Ok(Ok(_))) {
            return format!("I got partway through and then the DM stopped going through. Sent {idx} of {page_count} pages.");
        }
    }

    let missing = total - with_invite;
    if missing == 0 {
        format!("Sent. {total} servers, all with invites.")
    } else {
        format!("Sent. {total} servers, {with_invite} with invites. The other {missing} are in the list with the reason why.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, members: u64, invite: Result<String, String>) -> Entry {
        Entry {
            id: GuildId::new(1),
            name: name.into(),
            members: Some(members),
            owner: "owner#0001".into(),
            owner_id: Some(UserId::new(2)),
            created: 1_600_000_000,
            invite,
        }
    }

    #[test]
    fn a_short_list_fits_on_one_page() {
        let entries = vec![
            entry("Alpha", 120, Ok("https://discord.gg/aaa".into())),
            entry("Beta", 4, Err("no access".into())),
        ];
        let pages = build_pages(&entries);
        assert_eq!(pages.len(), 1);
        assert!(pages[0].contains("https://discord.gg/aaa"));
        assert!(pages[0].contains("No invite: no access"));
    }

    /// Every page has to clear Discord's 4096-character description limit, or
    /// the send fails and the owner gets a partial list with no explanation.
    #[test]
    fn a_long_list_is_split_into_sendable_pages() {
        let entries: Vec<Entry> = (0..200)
            .map(|n| entry(&format!("A fairly long server name number {n}"), n, Ok(format!("https://discord.gg/code{n}"))))
            .collect();
        let pages = build_pages(&entries);
        assert!(pages.len() > 1, "200 servers should not fit on one page");
        for p in &pages {
            assert!(p.chars().count() <= 4096, "a page was {} characters", p.chars().count());
        }
        // Nothing dropped in the splitting.
        let joined = pages.join("");
        assert!(joined.contains("https://discord.gg/code0"));
        assert!(joined.contains("https://discord.gg/code199"));
    }

    /// A lookup that never finishes (a request with no timeout, or a long
    /// rate-limit sleep) can't hold the list up: it's reported as timed out and
    /// the rest come back.
    #[tokio::test]
    async fn a_stuck_lookup_times_out_without_holding_up_the_rest() {
        let ms = Duration::from_millis;
        let out = bounded(
            vec![1u32, 2, 3],
            ms(100),
            Duration::from_secs(5),
            |n| async move {
                if n == 2 {
                    std::future::pending::<()>().await;
                }
                format!("{n} ok")
            },
            |n, why| format!("{n} {why}"),
        )
        .await;
        let mut out = out;
        out.sort();
        assert_eq!(out, vec!["1 ok".to_string(), format!("2 {SLOW}"), "3 ok".to_string()]);
    }

    /// And the whole list has a ceiling: whatever is still going then is
    /// listed as out of time rather than waited on.
    #[tokio::test]
    async fn the_whole_list_has_a_deadline() {
        let start = std::time::Instant::now();
        let out = bounded(
            vec![1u32, 2],
            Duration::from_secs(60),
            Duration::from_millis(150),
            |_| async { std::future::pending::<String>().await },
            |n, why| format!("{n} {why}"),
        )
        .await;
        assert!(start.elapsed() < Duration::from_secs(5), "it waited past the deadline");
        assert_eq!(out.len(), 2, "every server is still listed");
        assert!(out.iter().all(|e| e.ends_with(OUT_OF_TIME)), "{out:?}");
    }

    #[test]
    fn member_counts_are_readable() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
