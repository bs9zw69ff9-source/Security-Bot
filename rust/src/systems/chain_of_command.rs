//! Chain of Command boards.
//!
//! One embed per board, listing each group's roles (in hierarchy order) next
//! to whoever currently holds them. Posted once via `/chainofcommand setup`,
//! then kept in sync automatically as members' roles change.

use once_cell::sync::Lazy;
use serenity::builder::{CreateEmbed, CreateMessage, EditMessage};
use serenity::client::Context;
use serenity::model::id::{ChannelId, GuildId, MessageId, RoleId};
use serenity::model::Timestamp;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::common::embeds::is_unknown_message;
use crate::state::chain_of_command::{get_chain, get_chain_keys, update_chain, ChainGroup};

/// The title a board gets when it wasn't given one.
pub const DEFAULT_TITLE: &str = "🎖️  CHAIN OF COMMAND";
/// What untitled boards used to be called. Still matched by the sweep, so an
/// old copy left behind under that name gets cleared too.
const LEGACY_DEFAULT_TITLE: &str = "📋 Chain of Command";

fn effective_title(title: &str) -> &str {
    if title.is_empty() {
        DEFAULT_TITLE
    } else {
        title
    }
}

/// Whether an embed titled `t` is a copy of a board configured with `board_title`.
fn is_board_title(t: &str, board_title: &str) -> bool {
    t == effective_title(board_title) || (board_title.is_empty() && t == LEGACY_DEFAULT_TITLE)
}

/// One render or sweep at a time per guild. Boot, the debounced refresh and
/// slash commands can all render at once, and unserialized, two of them could
/// each find the board gone and each post a replacement.
static RENDER_LOCKS: Lazy<Mutex<HashMap<GuildId, Arc<tokio::sync::Mutex<()>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn render_lock(guild_id: GuildId) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = match RENDER_LOCKS.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    map.entry(guild_id).or_default().clone()
}

/// One member, reduced to what a board actually needs.
pub struct Holder {
    pub id: u64,
    pub name: String,
    pub roles: Vec<RoleId>,
}

/// Every member of the guild, fetched over HTTP.
///
/// It has to be HTTP rather than `ctx.cache`. Serenity's cache is only ever
/// filled by gateway events, and `GUILD_CREATE` sends just a slice of the
/// member list (`large_threshold`, 50 by default). Reading the cache here
/// meant most role holders simply weren't in it, and the board rendered
/// "(none)" under roles that plainly had people in them. Note that the HTTP
/// fetch does not populate the cache either - `Http` holds no reference to
/// it - so the returned members have to be used directly.
///
/// All or nothing: `None` unless every page arrived. A board rendered from a
/// partial or empty list shows "(none)" under roles people do hold, and since
/// every restart re-renders, a blip during boot would overwrite a correct
/// board with a wrong one. Leaving the board as it was is always better.
pub async fn fetch_holders(ctx: &Context, guild_id: GuildId) -> Option<Vec<Holder>> {
    use serenity::futures::StreamExt;

    // members_iter pages through in chunks of 1000; a plain members() call
    // would silently stop at the first page.
    let mut stream = Box::pin(guild_id.members_iter(&ctx.http));
    let mut out = Vec::new();
    while let Some(result) = stream.next().await {
        match result {
            Ok(m) => out.push(Holder {
                id: m.user.id.get(),
                name: m.user.name.to_string(),
                roles: m.roles.clone(),
            }),
            Err(e) => {
                eprintln!("⚠️ chain of command: member fetch for {guild_id} failed ({e}); leaving its boards as they are");
                return None;
            }
        }
    }
    // The bot is a member itself, so an empty list means the fetch came back
    // wrong, not that the server is empty.
    if out.is_empty() {
        eprintln!("⚠️ chain of command: member fetch for {guild_id} came back empty; leaving its boards as they are");
        return None;
    }
    Some(out)
}

pub fn build_chain_of_command_embed(
    ctx: &Context,
    guild_id: GuildId,
    groups: &[ChainGroup],
    title: &str,
    members: &[Holder],
) -> CreateEmbed {
    // Roles, unlike members, are sent complete in GUILD_CREATE, so the cache is
    // trustworthy here.
    let existing_roles: Vec<RoleId> =
        ctx.cache.guild(guild_id).map(|g| g.roles.keys().copied().collect()).unwrap_or_default();

    CreateEmbed::new()
        .color(crate::common::theme::palette::POWDER)
        .title(effective_title(title))
        .footer(crate::common::theme::footer("Chain of Command • updates itself as roles change"))
        .timestamp(Timestamp::now())
        .description(chain_description(groups, members, &existing_roles))
}

/// The board's description text. Split out from the embed so it can be tested
/// without a live `Context`.
fn chain_description(groups: &[ChainGroup], members: &[Holder], existing_roles: &[RoleId]) -> String {
    // Discord only resolves @mentions in an embed's description/field VALUE,
    // never in a field NAME - so roles have to live in the description
    // alongside their holders, not as field headers, or they render as raw
    // <@&id> text instead of an actual mention.
    let mut group_blocks: Vec<String> = Vec::new();

    // If the role list is somehow empty, skip the existence filter rather than
    // dropping every role and rendering an empty board.
    let filter_missing_roles = !existing_roles.is_empty();

    for group in groups {
        let mut role_blocks: Vec<String> = Vec::new();
        for role_id_str in &group.role_ids {
            let Ok(raw) = role_id_str.parse::<u64>() else { continue };
            let role = RoleId::new(raw);
            if filter_missing_roles && !existing_roles.contains(&role) {
                continue;
            }
            let mut holders: Vec<(String, u64)> = members
                .iter()
                .filter(|m| m.roles.contains(&role))
                .map(|m| (m.name.clone(), m.id))
                .collect();
            holders.sort_by(|a, b| a.0.cmp(&b.0));
            let body = if holders.is_empty() {
                "*(none)*".to_string()
            } else {
                holders.iter().map(|(_, id)| format!("<@{id}>")).collect::<Vec<_>>().join("\n")
            };
            role_blocks.push(format!("<@&{role}>\n{body}"));
        }
        if role_blocks.is_empty() {
            continue;
        }
        group_blocks.push(match &group.label {
            Some(label) => format!("**{label}**\n{}", role_blocks.join("\n\n")),
            None => role_blocks.join("\n\n"),
        });
    }

    if group_blocks.is_empty() {
        return "None of the roles on this board exist in the server any more.".to_string();
    }
    let joined = group_blocks.join("\n\n");
    // Discord counts the 4096 description limit in UTF-16 units.
    if joined.encode_utf16().count() <= 4096 {
        return joined;
    }
    let mut truncated = String::new();
    let mut used = 0usize;
    for ch in joined.chars() {
        let w = ch.len_utf16();
        if used + w > 4096 {
            break;
        }
        truncated.push(ch);
        used += w;
    }
    truncated
}

/// Post or refresh (edit-in-place) one board for a guild, if configured.
/// Safe to call often - a no-op when that key isn't set up yet. `Err` carries
/// a reason a slash command can pass on.
pub async fn render_chain_of_command(ctx: &Context, guild_id: GuildId, key: &str) -> Result<(), String> {
    // Only worth paying for the member fetch if this board is actually set up.
    let cfg = get_chain(&guild_id.to_string(), key);
    if cfg.channel_id.is_empty() || cfg.groups.is_empty() {
        return Ok(());
    }
    let Some(members) = fetch_holders(ctx, guild_id).await else {
        return Err("I couldn't read the member list just now, so I left the board as it was. Try `/chainofcommand refresh` in a moment.".to_string());
    };
    render_chain_of_command_with(ctx, guild_id, key, &members).await
}

/// Render one board against an already-fetched member list, so a guild with
/// several boards pays for one fetch rather than one per board.
async fn render_chain_of_command_with(ctx: &Context, guild_id: GuildId, key: &str, members: &[Holder]) -> Result<(), String> {
    let lock = render_lock(guild_id);
    let _turn = lock.lock().await;
    // Read after the wait, so this sees whatever the render before it saved.
    let gid = guild_id.to_string();
    let cfg = get_chain(&gid, key);
    if cfg.channel_id.is_empty() || cfg.groups.is_empty() {
        return Ok(());
    }
    let Ok(raw) = cfg.channel_id.parse::<u64>() else { return Ok(()) };
    let channel = ChannelId::new(raw);

    let embed = build_chain_of_command_embed(ctx, guild_id, &cfg.groups, &cfg.title, members);

    // A board is posted once and edited in place from then on. Only post a
    // fresh one when Discord confirms the old message is gone: boards
    // re-render on every tracked role change, so treating a failed lookup as
    // deletion would leave a trail of duplicate boards down the channel.
    if let Ok(raw_mid) = cfg.message_id.parse::<u64>() {
        let mid = MessageId::new(raw_mid);
        match channel.message(&ctx.http, mid).await {
            Ok(mut existing) => {
                return existing.edit(&ctx.http, EditMessage::new().embed(embed)).await.map_err(|e| {
                    eprintln!("⚠️ couldn't edit chain-of-command board `{key}` ({mid}): {e}");
                    format!("I couldn't update the board ({e}).")
                });
            }
            Err(e) if !is_unknown_message(&e) => {
                eprintln!("⚠️ couldn't check chain-of-command board {mid} ({e}); leaving it alone rather than posting another");
                return Err(format!("I couldn't check the board that's already posted ({e}), so I left it alone rather than post a second one."));
            }
            // Genuinely deleted, so fall through and post a replacement.
            Err(_) => {}
        }
    }
    let posted = match channel.send_message(&ctx.http, CreateMessage::new().embed(embed)).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("⚠️ couldn't post chain-of-command board `{key}` in {channel}: {e}");
            return Err(format!("I couldn't post the board in <#{channel}> ({e}). Check that I can send messages and embeds there."));
        }
    };
    // Adopt it only if the board still lives in this channel: `setup` can
    // move it while this was posting, and storing this id then would point
    // the board at a message in the wrong channel.
    let mut adopted = false;
    let saved = update_chain(&gid, key, |b| {
        if b.channel_id == cfg.channel_id {
            b.message_id = posted.id.to_string();
            adopted = true;
        }
    });
    if !adopted {
        let _ = posted.delete(&ctx.http).await;
        return Ok(());
    }
    if !saved {
        eprintln!("❌ posted chain-of-command board `{key}` as {} but couldn't save its id; a restart would post it again", posted.id);
        return Err("The board is up, but I couldn't save which message it is, so a restart would post a second copy. The bot's log has the reason.".to_string());
    }
    Ok(())
}

/// Clear earlier copies of a board in its channel, keeping the tracked one.
///
/// Boards carry no buttons, so unlike the ticket and application panels there
/// is no custom id to match on. The title is used instead, and only against
/// messages this bot posted in that board's own channel, which keeps it from
/// reaching anything else that happens to be there.
pub async fn sweep_duplicate_boards(ctx: &Context, guild_id: GuildId) {
    use serenity::builder::GetMessages;

    let lock = render_lock(guild_id);
    let _turn = lock.lock().await;
    let me = ctx.cache.current_user().id;
    let gid = guild_id.to_string();
    let keys = get_chain_keys(&gid);
    // Every board's live message, across all keys. Two boards can share a
    // channel and a title, and neither may sweep the other away.
    let tracked: HashSet<MessageId> = keys
        .iter()
        .filter_map(|k| get_chain(&gid, k).message_id.parse::<u64>().ok().map(MessageId::new))
        .collect();
    for key in keys {
        let cfg = get_chain(&gid, &key);
        let Ok(raw) = cfg.channel_id.parse::<u64>() else { continue };
        // With no tracked board there is nothing to keep, so any copy in the
        // channel might be the only one. Leave them all.
        if cfg.message_id.parse::<u64>().is_err() {
            continue;
        }
        let channel = ChannelId::new(raw);

        let Ok(messages) = channel.messages(&ctx.http, GetMessages::new().limit(100)).await else { continue };
        let mut removed = 0;
        for msg in messages {
            if msg.author.id != me || tracked.contains(&msg.id) {
                continue;
            }
            let matches = msg.embeds.iter().any(|e| e.title.as_deref().is_some_and(|t| is_board_title(t, &cfg.title)));
            if matches && msg.delete(&ctx.http).await.is_ok() {
                removed += 1;
            }
        }
        if removed > 0 {
            println!("🧹 Cleared {removed} stale chain-of-command board(s) in #{channel}");
        }
    }
}

/// Render every board configured for a guild - used on boot/join and after a
/// tracked role change, since either could touch any one of them. If the
/// member list can't be read, the boards are left as they are and it tries
/// once more a minute later.
pub async fn render_all_chains_of_command(ctx: &Context, guild_id: GuildId) {
    if render_all_once(ctx, guild_id).await {
        return;
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        render_all_once(&ctx, guild_id).await;
    });
}

/// `false` only when the member fetch failed and nothing was rendered. Each
/// board's own failure is already logged by the render.
async fn render_all_once(ctx: &Context, guild_id: GuildId) -> bool {
    let keys = get_chain_keys(&guild_id.to_string());
    if keys.is_empty() {
        return true;
    }
    let Some(members) = fetch_holders(ctx, guild_id).await else { return false };
    for key in keys {
        let _ = render_chain_of_command_with(ctx, guild_id, &key, &members).await;
    }
    true
}

/// Debounced per-guild refresh so a burst of role changes (e.g. a bulk sync)
/// collapses into one re-render instead of one edit per member.
static REFRESH_GEN: Lazy<Mutex<HashMap<String, u64>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub fn schedule_chain_of_command_refresh(ctx: &Context, guild_id: GuildId) {
    if get_chain_keys(&guild_id.to_string()).is_empty() {
        return;
    }
    let gid = guild_id.to_string();
    // Bump a generation counter; only the newest scheduled task renders, which
    // is the equivalent of clearing and resetting the JS timer.
    let my_gen = {
        let mut map = match REFRESH_GEN.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let g = map.entry(gid.clone()).or_insert(0);
        *g += 1;
        *g
    };

    let ctx2 = ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let still_current = {
            let map = match REFRESH_GEN.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            map.get(&gid).copied() == Some(my_gen)
        };
        if still_current {
            render_all_chains_of_command(&ctx2, guild_id).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(id: u64, name: &str, roles: &[u64]) -> Holder {
        Holder {
            id,
            name: name.to_string(),
            roles: roles.iter().map(|r| RoleId::new(*r)).collect(),
        }
    }

    fn group(label: Option<&str>, roles: &[u64]) -> ChainGroup {
        ChainGroup {
            label: label.map(|s| s.to_string()),
            role_ids: roles.iter().map(|r| r.to_string()).collect(),
        }
    }

    /// The reported bug: a role with someone in it rendered as "(none)".
    /// It happened because holders were read from a cache holding only a slice
    /// of the guild, so the regression test is simply that a member passed in
    /// actually shows up under their role.
    #[test]
    fn a_role_with_a_holder_does_not_render_as_none() {
        let groups = [group(None, &[100, 200])];
        let members = [holder(7, "alice", &[100])];
        let out = chain_description(&groups, &members, &[RoleId::new(100), RoleId::new(200)]);

        assert!(out.contains("<@&100>\n<@7>"), "role 100 should list its holder, got:\n{out}");
        assert!(out.contains("<@&200>\n*(none)*"), "role 200 is genuinely empty, got:\n{out}");
    }

    #[test]
    fn every_holder_of_a_role_is_listed_sorted_by_name() {
        let groups = [group(None, &[100])];
        let members = [
            holder(3, "carol", &[100]),
            holder(1, "alice", &[100]),
            holder(2, "bob", &[100, 200]),
        ];
        let out = chain_description(&groups, &members, &[RoleId::new(100)]);
        assert_eq!(out, "<@&100>\n<@1>\n<@2>\n<@3>");
    }

    #[test]
    fn labeled_groups_render_as_sub_headers() {
        let groups = [group(Some("Ranks"), &[100]), group(Some("Sub Classes"), &[200])];
        let members = [holder(1, "alice", &[100]), holder(2, "bob", &[200])];
        let out = chain_description(&groups, &members, &[RoleId::new(100), RoleId::new(200)]);
        assert_eq!(out, "**Ranks**\n<@&100>\n<@1>\n\n**Sub Classes**\n<@&200>\n<@2>");
    }

    /// A role deleted from the server is dropped from the board entirely.
    #[test]
    fn roles_that_no_longer_exist_are_skipped() {
        let groups = [group(None, &[100, 999])];
        let members = [holder(1, "alice", &[100])];
        let out = chain_description(&groups, &members, &[RoleId::new(100)]);
        assert_eq!(out, "<@&100>\n<@1>");
    }

    /// An empty role list means "I don't know what exists", not "nothing
    /// exists" - blanking the board there would be the same class of failure
    /// as the original bug.
    #[test]
    fn an_unknown_role_list_does_not_blank_the_board() {
        let groups = [group(None, &[100])];
        let members = [holder(1, "alice", &[100])];
        let out = chain_description(&groups, &members, &[]);
        assert_eq!(out, "<@&100>\n<@1>");
    }

    /// The sweep has to recognise the title a board is actually posted with.
    /// It used to look for the old default while boards went out under the
    /// new one, so leftover copies of untitled boards were never cleared.
    #[test]
    fn the_sweep_matches_the_title_boards_are_posted_with() {
        assert!(is_board_title(DEFAULT_TITLE, ""), "an untitled board posts as the default title");
        assert!(is_board_title(LEGACY_DEFAULT_TITLE, ""), "old copies of an untitled board are still swept");
        assert!(is_board_title("🚓 Police Chain of Command", "🚓 Police Chain of Command"));
        assert!(!is_board_title(LEGACY_DEFAULT_TITLE, "🚓 Police Chain of Command"), "a titled board only matches its own title");
        assert!(!is_board_title(DEFAULT_TITLE, "🚓 Police Chain of Command"));
    }

    #[test]
    fn description_is_truncated_to_the_discord_limit_in_utf16_units() {
        let role_ids: Vec<u64> = (1..=400).collect();
        let groups = [group(None, &role_ids)];
        let members: Vec<Holder> =
            (1..=400).map(|i| holder(i, &format!("user{i}"), &[i])).collect();
        let existing: Vec<RoleId> = role_ids.iter().map(|r| RoleId::new(*r)).collect();
        let out = chain_description(&groups, &members, &existing);
        assert!(out.encode_utf16().count() <= 4096, "must fit Discord's limit");
    }
}
