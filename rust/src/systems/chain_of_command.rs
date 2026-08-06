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

pub fn build_chain_of_command_embed(
    ctx: &Context,
    guild_id: GuildId,
    groups: &[ChainGroup],
    title: &str,
) -> CreateEmbed {
    // Discord only resolves @mentions in an embed's description/field VALUE,
    // never in a field NAME - so roles have to live in the description
    // alongside their holders, not as field headers, or they render as raw
    // <@&id> text instead of an actual mention.
    let mut group_blocks: Vec<String> = Vec::new();

    // Snapshot who holds what once, without holding the cache guard across an
    // await (there are none here, but the guard is still kept tight).
    let members: Vec<(u64, String, Vec<RoleId>)> = ctx
        .cache
        .guild(guild_id)
        .map(|g| {
            g.members
                .values()
                .map(|m| (m.user.id.get(), m.user.name.to_string(), m.roles.clone()))
                .collect()
        })
        .unwrap_or_default();
    let existing_roles: Vec<RoleId> =
        ctx.cache.guild(guild_id).map(|g| g.roles.keys().copied().collect()).unwrap_or_default();

    for group in groups {
        let mut role_blocks: Vec<String> = Vec::new();
        for role_id_str in &group.role_ids {
            let Ok(raw) = role_id_str.parse::<u64>() else { continue };
            let role = RoleId::new(raw);
            if !existing_roles.contains(&role) {
                continue;
            }
            let mut holders: Vec<(String, u64)> = members
                .iter()
                .filter(|(_, _, roles)| roles.contains(&role))
                .map(|(id, name, _)| (name.clone(), *id))
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

    let description = if group_blocks.is_empty() {
        "None of the configured roles exist in this server anymore.".to_string()
    } else {
        let joined = group_blocks.join("\n\n");
        // Discord counts the 4096 description limit in UTF-16 units.
        if joined.encode_utf16().count() > 4096 {
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
        } else {
            joined
        }
    };

    CreateEmbed::new()
        .color(colors::INFO)
        .title(if title.is_empty() { "📋 Chain of Command" } else { title })
        .timestamp(Timestamp::now())
        .description(description)
}

/// Post or refresh (edit-in-place) one board for a guild, if configured.
/// Safe to call often - a no-op when that key isn't set up yet.
pub async fn render_chain_of_command(ctx: &Context, guild_id: GuildId, key: &str) {
    let cfg = get_chain(&guild_id.to_string(), key);
    if cfg.channel_id.is_empty() || cfg.groups.is_empty() {
        return;
    }
    let Ok(raw) = cfg.channel_id.parse::<u64>() else { return };
    let channel = ChannelId::new(raw);

    // Without a full member fetch, the cache only holds whoever the bot has
    // already seen - most holders would be missing on a server it hasn't fully
    // cached yet.
    let _ = guild_id.members(&ctx.http, None, None).await;

    let embed = build_chain_of_command_embed(ctx, guild_id, &cfg.groups, &cfg.title);

    if !cfg.message_id.is_empty() {
        if let Ok(mid) = cfg.message_id.parse::<u64>() {
            if let Ok(mut existing) = channel.message(&ctx.http, MessageId::new(mid)).await {
                let _ = existing.edit(&ctx.http, EditMessage::new().embed(embed)).await;
                return;
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
    for key in get_chain_keys(&guild_id.to_string()) {
        render_chain_of_command(ctx, guild_id, &key).await;
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
