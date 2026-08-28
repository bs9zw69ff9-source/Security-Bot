//! `/servers`: DM the bot owner an invite to every server the bot is in.
//!
//! Reuses a permanent invite where the server already has one, so running this
//! repeatedly doesn't litter every server's invite list, and only falls back to
//! creating one when there's nothing suitable to reuse.

use serenity::all::*;
use serenity::builder::{CreateEmbed, CreateInvite, CreateMessage};

use crate::colors;

/// Where the list is sent. This is the bot owner, kept separate from
/// BOT_OWNER_IDS on purpose: that set is a trust list and can hold several
/// people, while this is the one inbox the list belongs in.
pub const REPORT_TO: u64 = 1014251293159731310;

/// How many channels to try before giving up on a server. A server that says no
/// to the first few is going to say no to the rest, and every attempt is another
/// request against the rate limit.
const CHANNEL_ATTEMPTS: usize = 5;

/// Discord's embed description limit is 4096; leave room for the header.
const PAGE_LIMIT: usize = 3800;

pub struct Entry {
    pub name: String,
    pub members: Option<u64>,
    /// The invite, or why there isn't one.
    pub invite: Result<String, String>,
}

/// Walk every server the bot is in and work out an invite for each.
pub async fn collect(ctx: &Context) -> Vec<Entry> {
    let guilds = ctx.cache.guilds();
    let mut out = Vec::with_capacity(guilds.len());
    for gid in guilds {
        // Cached name first, since it costs nothing. Falling back to HTTP keeps
        // a server that hasn't finished arriving in the cache from showing up
        // as a bare id.
        let cached = ctx.cache.guild(gid).map(|g| (g.name.clone(), g.member_count));
        let (name, members) = match cached {
            Some((n, c)) => (n, Some(c)),
            None => match gid.to_partial_guild(&ctx.http).await {
                Ok(g) => (g.name.clone(), g.approximate_member_count),
                Err(_) => (format!("Unknown server ({gid})"), None),
            },
        };
        out.push(Entry { name, members, invite: invite_for(ctx, gid).await });
    }
    // Biggest first, which is usually the order you care about.
    out.sort_by(|a, b| b.members.unwrap_or(0).cmp(&a.members.unwrap_or(0)));
    out
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
            Some(n) => format!(" ({} members)", thousands(n)),
            None => String::new(),
        };
        let line = match &e.invite {
            Ok(url) => format!("**{}**{}\n{}\n\n", e.name, members, url),
            Err(why) => format!("**{}**{}\nNo invite: {}\n\n", e.name, members, why),
        };
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
            format!("Servers I'm in ({} of {page_count})", idx + 1)
        } else {
            "Servers I'm in".to_string()
        };
        let mut e = CreateEmbed::new().color(colors::INFO).title(title).description(page);
        if idx + 1 == page_count {
            e = e.footer(CreateEmbedFooter::new(format!(
                "{total} servers, {with_invite} with an invite"
            )));
        }
        if dm.send_message(&ctx.http, CreateMessage::new().embed(e)).await.is_err() {
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
        Entry { name: name.into(), members: Some(members), invite }
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

    #[test]
    fn member_counts_are_readable() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
