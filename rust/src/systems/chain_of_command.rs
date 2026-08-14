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
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::embeds::colors;
use crate::state::chain_of_command::{get_chain, get_chain_keys, update_chain, ChainGroup};

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
pub async fn fetch_holders(ctx: &Context, guild_id: GuildId) -> Vec<Holder> {
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
                // Partial results still beat rendering an empty board, so keep
                // whatever arrived before the failure.
                eprintln!("⚠️ chain of command: member fetch stopped early: {e}");
                break;
            }
        }
    }
    out
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
        .color(colors::INFO)
        .title(if title.is_empty() { "📋 Chain of Command" } else { title })
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
        return "None of the configured roles exist in this server anymore.".to_string();
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
/// Safe to call often - a no-op when that key isn't set up yet.
pub async fn render_chain_of_command(ctx: &Context, guild_id: GuildId, key: &str) {
    // Only worth paying for the member fetch if this board is actually set up.
    let cfg = get_chain(&guild_id.to_string(), key);
    if cfg.channel_id.is_empty() || cfg.groups.is_empty() {
        return;
    }
    let members = fetch_holders(ctx, guild_id).await;
    render_chain_of_command_with(ctx, guild_id, key, &members).await;
}

/// Render one board against an already-fetched member list, so a guild with
/// several boards pays for one fetch rather than one per board.
async fn render_chain_of_command_with(ctx: &Context, guild_id: GuildId, key: &str, members: &[Holder]) {
    let cfg = get_chain(&guild_id.to_string(), key);
    if cfg.channel_id.is_empty() || cfg.groups.is_empty() {
        return;
    }
    let Ok(raw) = cfg.channel_id.parse::<u64>() else { return };
    let channel = ChannelId::new(raw);

    let embed = build_chain_of_command_embed(ctx, guild_id, &cfg.groups, &cfg.title, members);

    // A board is posted once and edited in place from then on. Only post a
    // fresh one when Discord confirms the old message is gone: boards
    // re-render on every tracked role change, so treating a failed lookup as
    // deletion would leave a trail of duplicate boards down the channel.
    if !cfg.message_id.is_empty() {
        if let Ok(raw_mid) = cfg.message_id.parse::<u64>() {
            let mid = MessageId::new(raw_mid);
            match channel.message(&ctx.http, mid).await {
                Ok(mut existing) => {
                    let _ = existing.edit(&ctx.http, EditMessage::new().embed(embed)).await;
                    return;
                }
                Err(e) if !crate::common::embeds::is_unknown_message(&e) => {
                    eprintln!("⚠️ couldn't check chain-of-command board {mid} ({e}); leaving it alone rather than posting another");
                    return;
                }
                // Genuinely deleted, so fall through and post a replacement.
                Err(_) => {}
            }
        }
    }
    if let Ok(posted) = channel.send_message(&ctx.http, CreateMessage::new().embed(embed)).await {
        update_chain(&guild_id.to_string(), key, |b| b.message_id = posted.id.to_string());
    }
}

/// Render every board configured for a guild - used on boot/join and after a
/// tracked role change, since either could touch any one of them.
pub async fn render_all_chains_of_command(ctx: &Context, guild_id: GuildId) {
    let keys = get_chain_keys(&guild_id.to_string());
    if keys.is_empty() {
        return;
    }
    let members = fetch_holders(ctx, guild_id).await;
    for key in keys {
        render_chain_of_command_with(ctx, guild_id, &key, &members).await;
    }
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
