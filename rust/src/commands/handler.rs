//! Slash command dispatch.

use serenity::builder::{
    CreateEmbed, CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage,
    EditInteractionResponse, GetMessages,
};
use serenity::client::Context;
use serenity::model::application::{CommandInteraction, ResolvedOption, ResolvedValue};
use serenity::model::id::{ChannelId, RoleId, UserId};
use serenity::model::{Permissions, Timestamp};

use crate::common::config::{now_ms, BOT_OWNER_IDS, CONFIG};
use crate::common::embeds::{
    alert_owner, build_bar, colors, embed, format_uptime, limit_denied_embed, render_anti_ping_response, sec_log,
    usage_footer,
};
use crate::common::guildinfo::{fetch_member, GuildInfo};
use crate::common::permissions::{can_act_on, is_mod, is_owner, is_whitelisted, try_dm};
use crate::state::anti_ping::{ap, AntiPing};
use crate::state::applications::{get_application, get_applications, update_application};
use crate::state::chain_of_command::{get_chain, get_chain_keys, update_chain, ChainGroup};
use crate::state::guild_settings::{gc, update as update_guild};
use crate::state::lockdown::{clear_lockdown, is_lockdown, locked_count, record_changes, set_lockdown};
use crate::state::mod_rates::{check_mod_limit, record_mod_action};
use crate::state::muted_roles::stashed_count;
use crate::state::tickets::{get_ticket_config, update_ticket_config, TicketType};
use crate::state::warnings::{add_warning, clear_warnings, get_warnings};
use crate::systems::anti_nuke::{bump_destructive, nuke_response, total_reason, Trip};
use crate::systems::applications::{apps_by_panel_channel, refresh_app_panel, render_channel_panel};
use crate::systems::chain_of_command::render_chain_of_command;
use crate::systems::mute::{lock_all_text_channels, mute_user, set_send_messages, unlock_all_text_channels, unmute_user};
use crate::systems::police_manual::build_police_manual_embed;
use crate::systems::setup_helpers::{build_setup_embed, quick_setup_guild};
use crate::systems::tickets::post_or_edit_panel;

const STAFF_ONLY: &str = "This one is staff only - you need the mod role.";
const OWNER_ONLY: &str = "This one's owner only.";
const NO_MUTE_ROLE: &str =
    "There's no mute role yet. Run `/setup quick`, or point me at one with `/setup roles mute_role:@Role`.";

// ── Option helpers ────────────────────────────────────────────
struct Opts<'a>(Vec<ResolvedOption<'a>>);

impl<'a> Opts<'a> {
    fn find(&self, name: &str) -> Option<&ResolvedValue<'a>> {
        self.0.iter().find(|o| o.name == name).map(|o| &o.value)
    }
    fn str(&self, name: &str) -> Option<&str> {
        match self.find(name) {
            Some(ResolvedValue::String(s)) => Some(s),
            _ => None,
        }
    }
    fn int(&self, name: &str) -> Option<i64> {
        match self.find(name) {
            Some(ResolvedValue::Integer(i)) => Some(*i),
            _ => None,
        }
    }
    fn boolean(&self, name: &str) -> Option<bool> {
        match self.find(name) {
            Some(ResolvedValue::Boolean(b)) => Some(*b),
            _ => None,
        }
    }
    fn user(&self, name: &str) -> Option<UserId> {
        match self.find(name) {
            Some(ResolvedValue::User(u, _)) => Some(u.id),
            _ => None,
        }
    }
    fn role(&self, name: &str) -> Option<RoleId> {
        match self.find(name) {
            Some(ResolvedValue::Role(r)) => Some(r.id),
            _ => None,
        }
    }
    fn channel(&self, name: &str) -> Option<ChannelId> {
        match self.find(name) {
            Some(ResolvedValue::Channel(c)) => Some(c.id),
            _ => None,
        }
    }
}

/// Flatten a command's options into (subcommand-group, subcommand, options).
fn dissect(options: Vec<ResolvedOption<'_>>) -> (Option<String>, Option<String>, Opts<'_>) {
    if let Some(first) = options.first() {
        let name = first.name.to_string();
        match &first.value {
            ResolvedValue::SubCommandGroup(inner) => {
                if let Some(sub) = inner.first() {
                    let sub_name = sub.name.to_string();
                    if let ResolvedValue::SubCommand(args) = &sub.value {
                        return (Some(name), Some(sub_name), Opts(args.clone()));
                    }
                    return (Some(name), Some(sub_name), Opts(vec![]));
                }
                return (Some(name), None, Opts(vec![]));
            }
            ResolvedValue::SubCommand(args) => return (None, Some(name), Opts(args.clone())),
            _ => {}
        }
    }
    (None, None, Opts(options))
}

// ── Response helpers ──────────────────────────────────────────
async fn reply(ctx: &Context, i: &CommandInteraction, msg: CreateInteractionResponseMessage) {
    let _ = i.create_response(ctx, CreateInteractionResponse::Message(msg)).await;
}
async fn reply_text(ctx: &Context, i: &CommandInteraction, text: &str) {
    reply(ctx, i, CreateInteractionResponseMessage::new().content(text).ephemeral(true)).await;
}
async fn reply_embed(ctx: &Context, i: &CommandInteraction, e: CreateEmbed, ephemeral: bool) {
    reply(ctx, i, CreateInteractionResponseMessage::new().embed(e).ephemeral(ephemeral)).await;
}
async fn defer(ctx: &Context, i: &CommandInteraction) {
    let _ = i
        .create_response(ctx, CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new().ephemeral(true)))
        .await;
}
async fn edit_text(ctx: &Context, i: &CommandInteraction, text: impl Into<String>) {
    let _ = i.edit_response(ctx, EditInteractionResponse::new().content(text.into())).await;
}
async fn edit_embed(ctx: &Context, i: &CommandInteraction, e: CreateEmbed) {
    let _ = i.edit_response(ctx, EditInteractionResponse::new().embed(e)).await;
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Owner-or-server-owner gate used by every configuration command.
fn is_privileged(user_id: UserId, owner_id: UserId) -> bool {
    is_owner(user_id) || user_id == owner_id
}

pub async fn handle(ctx: &Context, i: &CommandInteraction) {
    let Some(guild_id) = i.guild_id else {
        return reply_text(ctx, i, "You can only use this in a server.").await;
    };
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else { return };
    let Some(member) = fetch_member(ctx, guild_id, i.user.id).await else { return };
    let (group, subcmd, opts) = dissect(i.data.options());
    let gid = guild_id.to_string();
    let staff = is_mod(&member, info.owner_id);
    let exempt = is_whitelisted(&member, info.owner_id);
    let privileged = is_privileged(i.user.id, info.owner_id);

    match i.data.name.as_str() {
        // ── /mute ──────────────────────────────────────────────
        "mute" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let Some(target_id) = opts.user("user") else { return };
            let minutes = opts.int("minutes").unwrap_or(10);
            let reason = opts.str("reason").unwrap_or("No reason provided").to_string();
            let Some(target) = fetch_member(ctx, guild_id, target_id).await else {
                return reply_text(ctx, i, "I can't find that user in this server.").await;
            };
            if let Err(why) = can_act_on(&info, &member, &target) {
                return reply_text(ctx, i, &why).await;
            }
            let mute_role_ok = gc(&gid)
                .mute_role_id
                .parse::<u64>()
                .ok()
                .map(|r| info.roles.contains_key(&RoleId::new(r)))
                .unwrap_or(false);
            if !mute_role_ok {
                return reply_text(ctx, i, NO_MUTE_ROLE).await;
            }
            if !exempt {
                let c = check_mod_limit(&gid, &i.user.id.to_string(), "mute");
                if !c.allowed {
                    return reply_embed(ctx, i, limit_denied_embed("mute", c.used, c.limit, c.resets_in_min), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "mute");
            }
            if !mute_user(ctx, &info, &target, minutes, &reason).await {
                return reply_text(ctx, i, NO_MUTE_ROLE).await;
            }
            let c = check_mod_limit(&gid, &i.user.id.to_string(), "mute");
            let stashed = stashed_count(&gid, &target_id.to_string());
            let mut e = CreateEmbed::new()
                .color(colors::MUTED)
                .title("🔇 Member Muted")
                .description(format!(
                    "Muted <@{target_id}> for **{}**.\n**Reason:** {reason}\nI've set aside **{stashed}** role{} and will hand them back on unmute.",
                    if minutes > 0 { format!("{minutes} minutes") } else { "as long as it takes".to_string() },
                    plural(stashed)
                ))
                .timestamp(Timestamp::now());
            if !exempt {
                e = e.footer(CreateEmbedFooter::new(usage_footer("mute", c.used, c.limit)));
            }
            reply_embed(ctx, i, e, false).await;
        }

        // ── /unmute ────────────────────────────────────────────
        "unmute" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let Some(target_id) = opts.user("user") else {
                return reply_text(ctx, i, "I couldn't find that user.").await;
            };
            if gc(&gid).mute_role_id.is_empty() {
                return reply_text(ctx, i, NO_MUTE_ROLE).await;
            }
            let stashed = stashed_count(&gid, &target_id.to_string());
            unmute_user(ctx, guild_id, target_id, &format!("Manual unmute by {}", i.user.tag())).await;
            reply_embed(
                ctx,
                i,
                CreateEmbed::new()
                    .color(colors::SUCCESS)
                    .title("🔊 Member Unmuted")
                    .description(format!(
                        "<@{target_id}> is unmuted, and I gave back **{stashed}** stashed role{}.",
                        plural(stashed)
                    ))
                    .timestamp(Timestamp::now()),
                false,
            )
            .await;
        }

        // ── /kick ──────────────────────────────────────────────
        "kick" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let Some(target_id) = opts.user("user") else { return };
            let reason = opts.str("reason").unwrap_or("No reason provided").to_string();
            let Some(target) = fetch_member(ctx, guild_id, target_id).await else {
                return reply_text(ctx, i, "I can't find that user in this server.").await;
            };
            if let Err(why) = can_act_on(&info, &member, &target) {
                return reply_text(ctx, i, &why).await;
            }
            if !exempt {
                if let Some(trip) = bump_destructive(&gid, &i.user.id.to_string(), "kicks", CONFIG.nuke_kick_threshold) {
                    reply_text(ctx, i, "Hold on - that just tripped the anti-nuke protection.").await;
                    let reason = if trip == Trip::Category {
                        format!(
                            "Issued {}+ kicks via commands in {}s",
                            CONFIG.nuke_kick_threshold,
                            CONFIG.nuke_window_ms / 1000
                        )
                    } else {
                        total_reason()
                    };
                    return nuke_response(ctx, guild_id, i.user.id, &reason).await;
                }
                let c = check_mod_limit(&gid, &i.user.id.to_string(), "kick");
                if !c.allowed {
                    return reply_embed(ctx, i, limit_denied_embed("kick", c.used, c.limit, c.resets_in_min), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "kick");
            }
            try_dm(&ctx.http, target_id, &format!("You've been kicked from **{}**.\nReason: {reason}", info.name)).await;
            let _ = guild_id.kick_with_reason(&ctx.http, target_id, &reason).await;
            sec_log(
                ctx,
                guild_id,
                "Member Kicked",
                &format!("<@{}> kicked <@{target_id}> - {reason}", i.user.id),
                colors::DANGER,
            )
            .await;
            let c = check_mod_limit(&gid, &i.user.id.to_string(), "kick");
            let mut e = CreateEmbed::new()
                .color(colors::DANGER)
                .title("👢 Member Kicked")
                .description(format!("Kicked <@{target_id}>.\n**Reason:** {reason}"))
                .timestamp(Timestamp::now());
            if !exempt {
                e = e.footer(CreateEmbedFooter::new(usage_footer("kick", c.used, c.limit)));
            }
            reply_embed(ctx, i, e, false).await;
        }

        // ── /ban ───────────────────────────────────────────────
        "ban" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let Some(target_id) = opts.user("user") else { return };
            let reason = opts.str("reason").unwrap_or("No reason provided").to_string();
            let delete_days = opts.int("delete_days").unwrap_or(0).clamp(0, 7) as u8;
            let Some(target) = fetch_member(ctx, guild_id, target_id).await else {
                return reply_text(ctx, i, "I can't find that user in this server.").await;
            };
            if let Err(why) = can_act_on(&info, &member, &target) {
                return reply_text(ctx, i, &why).await;
            }
            if !exempt {
                if let Some(trip) = bump_destructive(&gid, &i.user.id.to_string(), "bans", CONFIG.nuke_ban_threshold) {
                    reply_text(ctx, i, "Hold on - that just tripped the anti-nuke protection.").await;
                    let reason = if trip == Trip::Category {
                        format!(
                            "Issued {}+ bans via commands in {}s",
                            CONFIG.nuke_ban_threshold,
                            CONFIG.nuke_window_ms / 1000
                        )
                    } else {
                        total_reason()
                    };
                    return nuke_response(ctx, guild_id, i.user.id, &reason).await;
                }
                let c = check_mod_limit(&gid, &i.user.id.to_string(), "ban");
                if !c.allowed {
                    return reply_embed(ctx, i, limit_denied_embed("ban", c.used, c.limit, c.resets_in_min), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "ban");
            }
            try_dm(&ctx.http, target_id, &format!("You've been banned from **{}**.\nReason: {reason}", info.name)).await;
            let _ = guild_id.ban_with_reason(&ctx.http, target_id, delete_days, &reason).await;
            let c = check_mod_limit(&gid, &i.user.id.to_string(), "ban");
            sec_log(
                ctx,
                guild_id,
                "Member Banned",
                &format!("<@{}> banned <@{target_id}> - {reason}", i.user.id),
                colors::DANGER,
            )
            .await;
            let mut e = CreateEmbed::new()
                .color(colors::DANGER)
                .title("🔨 Member Banned")
                .description(format!("Banned <@{target_id}>.\n**Reason:** {reason}"))
                .timestamp(Timestamp::now());
            if !exempt {
                e = e.footer(CreateEmbedFooter::new(usage_footer("ban", c.used, c.limit)));
            }
            reply_embed(ctx, i, e, false).await;
        }

        // ── /unban ─────────────────────────────────────────────
        "unban" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let user_id_raw = opts.str("user_id").unwrap_or("").trim().to_string();
            let reason = opts.str("reason").unwrap_or("No reason provided").to_string();
            let valid = user_id_raw.len() >= 17 && user_id_raw.len() <= 20 && user_id_raw.chars().all(|c| c.is_ascii_digit());
            if !valid {
                return reply_text(ctx, i, "That doesn't look like a valid user ID.").await;
            }
            let uid = UserId::new(user_id_raw.parse::<u64>().unwrap_or(0));
            if guild_id.bans(&ctx.http, None, None).await.map(|b| !b.iter().any(|x| x.user.id == uid)).unwrap_or(true) {
                return reply_text(ctx, i, "That user isn't banned.").await;
            }
            let _ = guild_id.unban(&ctx.http, uid).await;
            sec_log(
                ctx,
                guild_id,
                "Member Unbanned",
                &format!("<@{}> lifted the ban on `{user_id_raw}` - {reason}", i.user.id),
                colors::SUCCESS,
            )
            .await;
            reply_embed(
                ctx,
                i,
                CreateEmbed::new()
                    .color(colors::SUCCESS)
                    .title("♻️ Member Unbanned")
                    .description(format!("<@{user_id_raw}> (`{user_id_raw}`) is unbanned.\n**Reason:** {reason}"))
                    .timestamp(Timestamp::now()),
                false,
            )
            .await;
        }

        // ── /purge ─────────────────────────────────────────────
        "purge" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let count = opts.int("count").unwrap_or(0).clamp(0, 100) as usize;
            let filter_user = opts.user("user");
            if !exempt {
                let c = check_mod_limit(&gid, &i.user.id.to_string(), "purge");
                if !c.allowed {
                    return reply_embed(ctx, i, limit_denied_embed("purge", c.used, c.limit, c.resets_in_min), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "purge");
            }
            defer(ctx, i).await;
            let Ok(messages) = i.channel_id.messages(&ctx.http, GetMessages::new().limit(100)).await else {
                return edit_text(ctx, i, "I couldn't fetch the messages here to clear them.").await;
            };
            let to_delete: Vec<_> = messages
                .into_iter()
                .filter(|m| filter_user.map(|u| m.author.id == u).unwrap_or(true))
                .take(count)
                .map(|m| m.id)
                .collect();
            let n = if to_delete.is_empty() {
                0
            } else {
                match i.channel_id.delete_messages(&ctx.http, &to_delete).await {
                    Ok(()) => to_delete.len(),
                    Err(_) => 0,
                }
            };
            let from = filter_user.map(|u| format!(" from <@{u}>")).unwrap_or_default();
            sec_log(
                ctx,
                guild_id,
                "Purge",
                &format!("<@{}> cleared **{n}** message{} in <#{}>{from}.", i.user.id, plural(n), i.channel_id),
                colors::WARN,
            )
            .await;
            let c = check_mod_limit(&gid, &i.user.id.to_string(), "purge");
            let mut e = CreateEmbed::new()
                .color(colors::WARN)
                .title("🗑️ Messages Cleared")
                .description(format!("Cleared **{n}** message{}{from}.", plural(n)))
                .timestamp(Timestamp::now());
            if !exempt {
                e = e.footer(CreateEmbedFooter::new(usage_footer("purge", c.used, c.limit)));
            }
            edit_embed(ctx, i, e).await;
        }

        // ── /lockdown ──────────────────────────────────────────
        "lockdown" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let lock = opts.str("action").unwrap_or("lock") == "lock";
            let channel_id = opts.channel("channel").unwrap_or(i.channel_id);
            if lock && !exempt {
                if let Some(trip) = bump_destructive(&gid, &i.user.id.to_string(), "chLock", CONFIG.nuke_channel_threshold) {
                    reply_text(ctx, i, "Hold on - that just tripped the anti-nuke protection.").await;
                    let reason = if trip == Trip::Category {
                        format!(
                            "Locked {}+ channels via commands in {}s",
                            CONFIG.nuke_channel_threshold,
                            CONFIG.nuke_window_ms / 1000
                        )
                    } else {
                        total_reason()
                    };
                    return nuke_response(ctx, guild_id, i.user.id, &reason).await;
                }
                let c = check_mod_limit(&gid, &i.user.id.to_string(), "lockdown");
                if !c.allowed {
                    return reply_embed(ctx, i, limit_denied_embed("lockdown", c.used, c.limit, c.resets_in_min), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "lockdown");
            }
            if let Ok(channels) = guild_id.channels(&ctx.http).await {
                if let Some(ch) = channels.get(&channel_id) {
                    set_send_messages(ctx, ch, RoleId::new(guild_id.get()), if lock { Some(false) } else { None }).await;
                }
            }
            sec_log(
                ctx,
                guild_id,
                if lock { "Channel Locked" } else { "Channel Unlocked" },
                &format!(
                    "<@{}> {} <#{channel_id}>.",
                    i.user.id,
                    if lock { "locked down" } else { "reopened" }
                ),
                if lock { colors::DANGER } else { colors::SUCCESS },
            )
            .await;
            let c = check_mod_limit(&gid, &i.user.id.to_string(), "lockdown");
            let mut e = CreateEmbed::new()
                .color(if lock { colors::DANGER } else { colors::SUCCESS })
                .title(if lock { "🔒 Channel Locked" } else { "🔓 Channel Unlocked" })
                .description(format!(
                    "<#{channel_id}> is now {}.",
                    if lock { "locked down - only staff can send messages" } else { "back open" }
                ))
                .timestamp(Timestamp::now());
            if lock && !exempt {
                e = e.footer(CreateEmbedFooter::new(usage_footer("lockdown", c.used, c.limit)));
            }
            reply_embed(ctx, i, e, false).await;
        }

        // ── /panic (owner only) - toggles: run again to lift ────
        "panic" => {
            if !privileged {
                return reply_text(ctx, i, OWNER_ONLY).await;
            }
            defer(ctx, i).await;
            if is_lockdown(&gid) {
                let unlocked = unlock_all_text_channels(ctx, guild_id).await;
                clear_lockdown(&gid);
                alert_owner(
                    ctx,
                    guild_id,
                    &format!("<@{}> lifted the panic lockdown. **{unlocked}** channels are back open.", i.user.id),
                    colors::SUCCESS,
                    "Panic Lockdown Lifted",
                )
                .await;
                return edit_text(
                    ctx,
                    i,
                    format!("Done - panic lockdown lifted and **{unlocked}** text channels are back open."),
                )
                .await;
            }
            set_lockdown(&gid, "panic", None);
            let outcome = lock_all_text_channels(ctx, guild_id).await;
            let locked = outcome.locked;
            record_changes(&gid, outcome.changes);
            alert_owner(
                ctx,
                guild_id,
                &format!(
                    "<@{}> hit the panic button and locked down **{locked}** channels. Run `/panic` again to lift it.",
                    i.user.id
                ),
                colors::NUKE,
                "Panic Lockdown",
            )
            .await;
            edit_text(ctx, i, format!("Panic lockdown is on - I've locked **{locked}** text channels. Run `/panic` again to lift it.")).await;
        }

        // ── /warn ──────────────────────────────────────────────
        "warn" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let Some(target_id) = opts.user("user") else { return };
            let reason = opts.str("reason").unwrap_or("No reason provided").to_string();
            let Some(target) = fetch_member(ctx, guild_id, target_id).await else {
                return reply_text(ctx, i, "I can't find that user in this server.").await;
            };
            if let Err(why) = can_act_on(&info, &member, &target) {
                return reply_text(ctx, i, &why).await;
            }
            if !exempt {
                let c = check_mod_limit(&gid, &i.user.id.to_string(), "warn");
                if !c.allowed {
                    return reply_embed(ctx, i, limit_denied_embed("warn", c.used, c.limit, c.resets_in_min), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "warn");
            }
            let total = add_warning(&gid, &target_id.to_string(), &reason, &i.user.id.to_string());
            try_dm(
                &ctx.http,
                target_id,
                &format!("You've picked up a warning in **{}** (that's #{total}). Reason: {reason}", info.name),
            )
            .await;
            sec_log(
                ctx,
                guild_id,
                "Warning Issued",
                &format!("<@{}> warned <@{target_id}> - that's **{total}** now. Reason: {reason}", i.user.id),
                colors::WARN,
            )
            .await;

            // Escalation
            let mut escalation = String::new();
            if CONFIG.warn_ban_at > 0 && total >= CONFIG.warn_ban_at {
                let _ = guild_id
                    .ban_with_reason(&ctx.http, target_id, 0, &format!("Auto-escalation: reached {total} warnings"))
                    .await;
                escalation = format!("\n🔨 That hit **{total}** warnings, so they've been auto-banned.");
                sec_log(
                    ctx,
                    guild_id,
                    "Auto-Escalation",
                    &format!("<@{target_id}> hit {total} warnings and was auto-banned."),
                    colors::DANGER,
                )
                .await;
            } else if CONFIG.warn_kick_at > 0 && total >= CONFIG.warn_kick_at {
                let _ = guild_id
                    .kick_with_reason(&ctx.http, target_id, &format!("Auto-escalation: reached {total} warnings"))
                    .await;
                escalation = format!("\n👢 That hit **{total}** warnings, so they've been auto-kicked.");
                sec_log(
                    ctx,
                    guild_id,
                    "Auto-Escalation",
                    &format!("<@{target_id}> hit {total} warnings and was auto-kicked."),
                    colors::DANGER,
                )
                .await;
            } else if CONFIG.warn_mute_at > 0 && total >= CONFIG.warn_mute_at {
                mute_user(
                    ctx,
                    &info,
                    &target,
                    CONFIG.warn_mute_min,
                    &format!("Auto-escalation: reached {total} warnings"),
                )
                .await;
                escalation = format!(
                    "\n🔇 That hit **{total}** warnings, so they've been auto-muted for {} min.",
                    CONFIG.warn_mute_min
                );
            }

            let c = check_mod_limit(&gid, &i.user.id.to_string(), "warn");
            let mut e = CreateEmbed::new()
                .color(colors::WARN)
                .title("⚠️ Warning Issued")
                .description(format!(
                    "Warned <@{target_id}>. **That's {total} in total.**\n**Reason:** {reason}{escalation}"
                ))
                .timestamp(Timestamp::now());
            if !exempt {
                e = e.footer(CreateEmbedFooter::new(usage_footer("warn", c.used, c.limit)));
            }
            reply_embed(ctx, i, e, false).await;
        }

        // ── /warnings ──────────────────────────────────────────
        "warnings" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let Some(target_id) = opts.user("user") else { return };
            let list = get_warnings(&gid, &target_id.to_string());
            if list.is_empty() {
                return reply_text(ctx, i, &format!("<@{target_id}> has a clean slate - no warnings.")).await;
            }
            let tag = target_id.to_user(&ctx.http).await.map(|u| u.tag()).unwrap_or_else(|_| target_id.to_string());
            let lines = list
                .iter()
                .rev()
                .take(15)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .enumerate()
                .map(|(idx, w)| format!("**{}.** {} - by <@{}> · <t:{}:R>", idx + 1, w.reason, w.by, w.at / 1000))
                .collect::<Vec<_>>()
                .join("\n");
            reply_embed(
                ctx,
                i,
                CreateEmbed::new()
                    .color(colors::WARN)
                    .title(format!("⚠️ Warnings for {tag}"))
                    .description(format!("**{} in total.**\n\n{lines}", list.len()))
                    .footer(CreateEmbedFooter::new(format!(
                        "Auto-actions kick in at: mute@{} · kick@{} · ban@{}",
                        CONFIG.warn_mute_at, CONFIG.warn_kick_at, CONFIG.warn_ban_at
                    )))
                    .timestamp(Timestamp::now()),
                true,
            )
            .await;
        }

        // ── /clearwarns ────────────────────────────────────────
        "clearwarns" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let Some(target_id) = opts.user("user") else { return };
            let had = get_warnings(&gid, &target_id.to_string()).len();
            clear_warnings(&gid, &target_id.to_string());
            sec_log(
                ctx,
                guild_id,
                "Warnings Cleared",
                &format!("<@{}> wiped **{had}** warning{} for <@{target_id}>.", i.user.id, plural(had)),
                colors::SUCCESS,
            )
            .await;
            reply_embed(
                ctx,
                i,
                embed(
                    colors::SUCCESS,
                    format!("Cleared **{had}** warning{} for <@{target_id}>. Clean slate.", plural(had)),
                    Some("Warnings Cleared"),
                ),
                true,
            )
            .await;
        }

        // ── /limits ────────────────────────────────────────────
        "limits" => {
            if !staff {
                return reply_text(ctx, i, STAFF_ONLY).await;
            }
            let window_hours = CONFIG.mod_window_ms / 3_600_000;
            if exempt {
                return reply_embed(
                    ctx,
                    i,
                    CreateEmbed::new()
                        .color(colors::INFO)
                        .title("🛡️ Your Mod Limits")
                        .description("You're whitelisted, so none of the rate limits apply to you.")
                        .timestamp(Timestamp::now()),
                    true,
                )
                .await;
            }
            let actions = [
                ("ban", "🔨", "Bans"),
                ("kick", "👢", "Kicks"),
                ("mute", "🔇", "Mutes"),
                ("warn", "⚠️", "Warns"),
                ("purge", "🗑️", "Purges"),
                ("lockdown", "🔒", "Lockdowns"),
            ];
            let mut e = CreateEmbed::new()
                .color(colors::INFO)
                .title("📊 Your Mod Action Limits")
                .description(format!(
                    "Here's where you're at over the last **{window_hours}h**. These top back up on their own as older actions age out."
                ))
                .timestamp(Timestamp::now());
            for (key, emoji, label) in actions {
                let c = check_mod_limit(&gid, &i.user.id.to_string(), key);
                let bar = build_bar(c.used, c.limit, 8);
                let pct = if c.limit == 0 { 0 } else { (c.used * 100) / c.limit };
                let warn = if c.remaining == 0 {
                    " 🚫"
                } else if c.remaining <= (c.limit as f64 * 0.2).ceil() as usize {
                    " ⚠️"
                } else {
                    ""
                };
                e = e.field(
                    format!("{emoji} {label}{warn}"),
                    format!("`{bar}` **{}/{}** used ({pct}%) - **{}** remaining", c.used, c.limit, c.remaining),
                    false,
                );
            }
            reply_embed(ctx, i, e, true).await;
        }

        // ── /antiping ──────────────────────────────────────────
        "antiping" => {
            if !privileged {
                return reply_text(ctx, i, "Only the bot owner or the server owner can change these settings.").await;
            }
            let a = ap(&gid);
            match subcmd.as_deref().unwrap_or("") {
                "status" => {
                    reply_embed(
                        ctx,
                        i,
                        CreateEmbed::new()
                            .color(if a.enabled { colors::SUCCESS } else { colors::NEUTRAL })
                            .title("📡 Anti-Ping - Status")
                            .field("Enabled", if a.enabled { "✅ On" } else { "⛔ Off" }, true)
                            .field("Action", format!("`{}`", a.action), true)
                            .field("Duration", format!("{} min", a.timeout_min), true)
                            .field("Delete message", if a.delete_message { "Yes" } else { "No" }, true)
                            .field("Ignore replies", if a.ignore_replies { "Yes" } else { "No" }, true)
                            .field("Channel notice", if a.notify_channel { "On" } else { "Off" }, true)
                            .field("Response", format!("```{}```", a.response_template), false)
                            .field("Protected users", id_list(&a.protected_users, "<@"), false)
                            .field("Protected roles", id_list(&a.protected_roles, "<@&"), false)
                            .timestamp(Timestamp::now()),
                        true,
                    )
                    .await;
                }
                "toggle" => {
                    let enabled = opts.boolean("enabled").unwrap_or(true);
                    crate::state::anti_ping::update(&gid, |c| c.enabled = enabled);
                    reply_embed(
                        ctx,
                        i,
                        embed(
                            if enabled { colors::SUCCESS } else { colors::NEUTRAL },
                            format!("Anti-ping is now **{}**.", if enabled { "enabled" } else { "disabled" }),
                            Some("Anti-Ping"),
                        ),
                        true,
                    )
                    .await;
                }
                "action" => {
                    let action = opts.str("type").unwrap_or("timeout").to_string();
                    crate::state::anti_ping::update(&gid, |c| c.action = action.clone());
                    reply_embed(ctx, i, embed(colors::INFO, format!("Punishment set to **{action}**."), Some("Anti-Ping")), true).await;
                }
                "duration" => {
                    let minutes = opts.int("minutes").unwrap_or(5);
                    crate::state::anti_ping::update(&gid, |c| c.timeout_min = minutes);
                    reply_embed(ctx, i, embed(colors::INFO, format!("Mute/timeout duration set to **{minutes} min**."), Some("Anti-Ping")), true).await;
                }
                "delete" => {
                    let v = opts.boolean("enabled").unwrap_or(false);
                    crate::state::anti_ping::update(&gid, |c| c.delete_message = v);
                    reply_embed(ctx, i, embed(colors::INFO, format!("Offending messages will {}.", if v { "**be deleted**" } else { "**not be deleted**" }), Some("Anti-Ping")), true).await;
                }
                "ignorereplies" => {
                    let v = opts.boolean("enabled").unwrap_or(true);
                    crate::state::anti_ping::update(&gid, |c| c.ignore_replies = v);
                    reply_embed(ctx, i, embed(colors::INFO, format!("Reply-pings will {}.", if v { "**be ignored**" } else { "**be punished**" }), Some("Anti-Ping")), true).await;
                }
                "response" => {
                    let text = opts.str("text").unwrap_or("default");
                    let template = if text.eq_ignore_ascii_case("default") {
                        AntiPing::defaults().response_template
                    } else {
                        text.to_string()
                    };
                    crate::state::anti_ping::update(&gid, |c| c.response_template = template.clone());
                    let preview = render_anti_ping_response(
                        &template,
                        &i.user.id.to_string(),
                        "@ProtectedUser",
                        &format!("timed out for {} min", a.timeout_min),
                    );
                    reply_embed(ctx, i, embed(colors::INFO, format!("Response template updated.\n\n**Template:**\n```{template}```\n**Preview:**\n{preview}\n\n_Placeholders: `{{user}}`, `{{targets}}`, `{{action}}`._"), Some("Anti-Ping")), true).await;
                }
                "notify" => {
                    let v = opts.boolean("enabled").unwrap_or(true);
                    crate::state::anti_ping::update(&gid, |c| c.notify_channel = v);
                    reply_embed(ctx, i, embed(colors::INFO, format!("Public channel warning is now **{}**.", if v { "on" } else { "off" }), Some("Anti-Ping")), true).await;
                }
                "protect" => {
                    let action = opts.str("action").unwrap_or("add");
                    let Some(user) = opts.user("user") else { return };
                    let id = user.to_string();
                    if action == "add" && a.protected_users.contains(&id) {
                        return reply_text(ctx, i, &format!("⚠️ <@{id}> is already protected.")).await;
                    }
                    crate::state::anti_ping::update(&gid, |c| {
                        if action == "add" {
                            c.protected_users.push(id.clone());
                        } else {
                            c.protected_users.retain(|x| *x != id);
                        }
                    });
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("<@{user}> {} from pings.", if action == "add" { "is now **protected**" } else { "is **no longer protected**" }), Some("Anti-Ping")), true).await;
                }
                "protectrole" => {
                    let action = opts.str("action").unwrap_or("add");
                    let Some(role) = opts.role("role") else { return };
                    let id = role.to_string();
                    if action == "add" && a.protected_roles.contains(&id) {
                        return reply_text(ctx, i, &format!("⚠️ <@&{id}> is already protected.")).await;
                    }
                    crate::state::anti_ping::update(&gid, |c| {
                        if action == "add" {
                            c.protected_roles.push(id.clone());
                        } else {
                            c.protected_roles.retain(|x| *x != id);
                        }
                    });
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("<@&{role}> {} from pings.", if action == "add" { "is now **protected**" } else { "is **no longer protected**" }), Some("Anti-Ping")), true).await;
                }
                "list" => {
                    reply_embed(
                        ctx,
                        i,
                        CreateEmbed::new()
                            .color(colors::INFO)
                            .title("📡 Anti-Ping - Protected")
                            .field("Users", newline_list(&a.protected_users, "<@"), true)
                            .field("Roles", newline_list(&a.protected_roles, "<@&"), true)
                            .timestamp(Timestamp::now()),
                        true,
                    )
                    .await;
                }
                _ => {}
            }
        }

        // ── /setup ─────────────────────────────────────────────
        "setup" => {
            if !privileged {
                return reply_text(ctx, i, "Only the bot owner or the server owner can change these settings.").await;
            }
            match subcmd.as_deref().unwrap_or("") {
                "quick" => {
                    defer(ctx, i).await;
                    let mod_role = opts.role("mod_role");
                    let r = quick_setup_guild(ctx, guild_id, mod_role).await;
                    let mut e = build_setup_embed(guild_id, &info.name, &[]);
                    e = e.title(format!("🛡️ Guardian quick setup - {}", info.name)).description(format!(
                        "{}{}\nCurrent settings:",
                        if r.created.is_empty() { String::new() } else { format!("**Created:** {}\n", r.created.join(", ")) },
                        if r.reused.is_empty() { String::new() } else { format!("**Reused existing:** {}\n", r.reused.join(", ")) },
                    ));
                    edit_embed(ctx, i, e).await;
                }
                "view" => reply_embed(ctx, i, build_setup_embed(guild_id, &info.name, &[]), true).await,
                "roles" => {
                    let mut changes = Vec::new();
                    if let Some(r) = opts.role("mod_role") {
                        update_guild(&gid, |s| s.mod_role_id = r.to_string());
                        changes.push(format!("Mod role → <@&{r}>"));
                    }
                    if let Some(r) = opts.role("mute_role") {
                        update_guild(&gid, |s| s.mute_role_id = r.to_string());
                        changes.push(format!("Mute role → <@&{r}> _(make sure it denies Send Messages)_"));
                    }
                    if changes.is_empty() {
                        return reply_text(ctx, i, "Give me at least one role to set.").await;
                    }
                    reply_embed(ctx, i, build_setup_embed(guild_id, &info.name, &changes), true).await;
                }
                "channels" => {
                    let mut changes = Vec::new();
                    if let Some(c) = opts.channel("log_channel") {
                        update_guild(&gid, |s| s.log_channel_id = c.to_string());
                        changes.push(format!("Log channel → <#{c}>"));
                    }
                    if let Some(c) = opts.channel("alert_channel") {
                        update_guild(&gid, |s| s.alert_channel_id = c.to_string());
                        changes.push(format!("Alert channel → <#{c}>"));
                    }
                    if let Some(c) = opts.channel("msg_log_channel") {
                        update_guild(&gid, |s| s.msg_log_channel_id = c.to_string());
                        changes.push(format!("Msg log → <#{c}>"));
                    }
                    if changes.is_empty() {
                        return reply_text(ctx, i, "Give me at least one channel to set.").await;
                    }
                    reply_embed(ctx, i, build_setup_embed(guild_id, &info.name, &changes), true).await;
                }
                "whitelist" => {
                    let action = opts.str("action").unwrap_or("add");
                    let user = opts.user("user");
                    let role = opts.role("role");
                    if user.is_none() && role.is_none() {
                        return reply_text(ctx, i, "Give me a user or a role.").await;
                    }
                    let mut changes = Vec::new();
                    if let Some(u) = user {
                        let id = u.to_string();
                        update_guild(&gid, |s| {
                            if action == "add" {
                                if !s.nuke_whitelist_user_ids.contains(&id) {
                                    s.nuke_whitelist_user_ids.push(id.clone());
                                }
                            } else {
                                s.nuke_whitelist_user_ids.retain(|x| *x != id);
                            }
                        });
                        changes.push(format!("Whitelist {}user <@{u}>", if action == "add" { "+" } else { "−" }));
                    }
                    if let Some(r) = role {
                        let id = r.to_string();
                        update_guild(&gid, |s| {
                            if action == "add" {
                                if !s.nuke_whitelist_role_ids.contains(&id) {
                                    s.nuke_whitelist_role_ids.push(id.clone());
                                }
                            } else {
                                s.nuke_whitelist_role_ids.retain(|x| *x != id);
                            }
                        });
                        changes.push(format!("Whitelist {}role <@&{r}>", if action == "add" { "+" } else { "−" }));
                    }
                    reply_embed(ctx, i, build_setup_embed(guild_id, &info.name, &changes), true).await;
                }
                "failsafe" => {
                    let action = opts.str("action").unwrap_or("add");
                    let Some(r) = opts.role("role") else { return };
                    let id = r.to_string();
                    update_guild(&gid, |s| {
                        if action == "add" {
                            if !s.failsafe_role_ids.contains(&id) {
                                s.failsafe_role_ids.push(id.clone());
                            }
                        } else {
                            s.failsafe_role_ids.retain(|x| *x != id);
                        }
                    });
                    let changes =
                        vec![format!("Failsafe {}role <@&{r}>", if action == "add" { "+" } else { "−" })];
                    reply_embed(ctx, i, build_setup_embed(guild_id, &info.name, &changes), true).await;
                }
                _ => {}
            }
        }

        // ── /config ────────────────────────────────────────────
        "config" => {
            if !privileged {
                return reply_text(ctx, i, "Only the bot owner or the server owner can view the config.").await;
            }
            let window_hours = CONFIG.mod_window_ms / 3_600_000;
            let g = gc(&gid);
            let a = ap(&gid);
            let e = CreateEmbed::new()
                .title("🛡️ Guardian Bot - Configuration")
                .color(colors::INFO)
                .field("🔧 Infrastructure", "\u{200b}", false)
                .field("Owner(s)", BOT_OWNER_IDS.iter().map(|id| format!("<@{id}>")).collect::<Vec<_>>().join(", "), true)
                .field("Log Channel", opt_channel(&g.log_channel_id), true)
                .field("Alert Channel", if g.alert_channel_id.is_empty() { "(uses log)".into() } else { format!("<#{}>", g.alert_channel_id) }, true)
                .field("Msg Log", opt_channel(&g.msg_log_channel_id), true)
                .field("Mute Role", opt_role(&g.mute_role_id), true)
                .field("Mod Role", opt_role(&g.mod_role_id), true)
                .field("🏅 Nuke Whitelist Roles", id_list(&g.nuke_whitelist_role_ids, "<@&"), false)
                .field("🏅 Nuke Whitelist Users", id_list(&g.nuke_whitelist_user_ids, "<@"), false)
                .field("💬 Anti-Spam", format!("{} msgs / {}ms · mention≥{} · dupes≥{} · invites {} → {} min mute", CONFIG.spam_threshold, CONFIG.spam_window_ms, CONFIG.spam_mention_limit, CONFIG.spam_duplicate_limit, if CONFIG.spam_block_invites { "blocked" } else { "allowed" }, CONFIG.spam_mute_min), false)
                .field("🚪 Anti-Raid", format!("{} joins / {}ms → {} min lockdown · new-acct kick: {}", CONFIG.raid_join_threshold, CONFIG.raid_window_ms, CONFIG.raid_lockdown_min, if CONFIG.raid_kick_new_on_lock { format!("<{}m", CONFIG.raid_min_account_age_min) } else { "off".into() }), false)
                .field("📡 Anti-Ping", format!("{} • `{}` • {} min • {} users / {} roles", if a.enabled { "On" } else { "Off" }, a.action, a.timeout_min, a.protected_users.len(), a.protected_roles.len()), false)
                .field("💣 Anti-Nuke (fast window)", format!("Window: {}ms", CONFIG.nuke_window_ms), false)
                .field("Chan Del/Create", format!("≥ {} / {}", CONFIG.nuke_channel_threshold, CONFIG.nuke_channel_create_thresh), true)
                .field("Role Del/Create", format!("≥ {} / {}", CONFIG.nuke_role_threshold, CONFIG.nuke_role_create_thresh), true)
                .field("Bans / Kicks", format!("≥ {} / {}", CONFIG.nuke_ban_threshold, CONFIG.nuke_kick_threshold), true)
                .field("Webhooks", format!("≥ {}", CONFIG.nuke_webhook_threshold), true)
                .field("Bot add", CONFIG.nuke_bot_add_action.clone(), true)
                .field("Any mix", if CONFIG.nuke_total_threshold > 0 { format!("≥ {}", CONFIG.nuke_total_threshold) } else { "off".to_string() }, true)
                .field("⚠️ Warn Escalation", format!("mute @ {} ({}m) · kick @ {} · ban @ {}", CONFIG.warn_mute_at, CONFIG.warn_mute_min, CONFIG.warn_kick_at, CONFIG.warn_ban_at), false)
                .field(format!("📊 Mod Daily Limits ({window_hours}h - whitelisted exempt)"), "\u{200b}", false)
                .field("🔨 Bans", CONFIG.mod_ban_limit.to_string(), true)
                .field("👢 Kicks", CONFIG.mod_kick_limit.to_string(), true)
                .field("🔇 Mutes", CONFIG.mod_mute_limit.to_string(), true)
                .field("⚠️ Warns", CONFIG.mod_warn_limit.to_string(), true)
                .field("🗑️ Purges", CONFIG.mod_purge_limit.to_string(), true)
                .field("🔒 Lockdowns", CONFIG.mod_lockdown_limit.to_string(), true)
                .footer(CreateEmbedFooter::new("These live in .env. Change them there and restart to pick them up."))
                .timestamp(Timestamp::now());
            reply_embed(ctx, i, e, true).await;
        }

        // ── /nuketest ──────────────────────────────────────────
        "nuketest" => {
            if !privileged {
                return reply_text(ctx, i, OWNER_ONLY).await;
            }
            let me = ctx.cache.current_user().id;
            let my_perms = ctx
                .cache
                .guild(guild_id)
                .and_then(|g| g.members.get(&me).map(|m| g.member_permissions(m)))
                .unwrap_or_else(Permissions::empty);
            let need = [
                ("View Audit Log", Permissions::VIEW_AUDIT_LOG),
                ("Ban Members", Permissions::BAN_MEMBERS),
                ("Kick Members", Permissions::KICK_MEMBERS),
                ("Manage Roles", Permissions::MANAGE_ROLES),
                ("Manage Channels", Permissions::MANAGE_CHANNELS),
                ("Moderate Members", Permissions::MODERATE_MEMBERS),
            ];
            let status = need
                .iter()
                .map(|(n, p)| format!("{} {n}", if my_perms.contains(*p) { "✅" } else { "❌" }))
                .collect::<Vec<_>>()
                .join("\n");
            reply_embed(
                ctx,
                i,
                CreateEmbed::new()
                    .color(colors::SUCCESS)
                    .title("✅ Anti-Nuke Active")
                    .description(format!("Anti-nuke is up and watching the audit log.\n\n**My permissions:**\n{status}"))
                    .timestamp(Timestamp::now()),
                true,
            )
            .await;
        }

        // ── /status ────────────────────────────────────────────
        "status" => {
            if !privileged {
                return reply_text(ctx, i, OWNER_ONLY).await;
            }
            let uptime = crate::START_TIME.get().map(|t| now_ms() - t).unwrap_or(0);
            let latency = crate::shard_latency(ctx.shard_id).await;
            reply_embed(
                ctx,
                i,
                CreateEmbed::new()
                    .color(colors::INFO)
                    .title("📊 Guardian Bot - Status")
                    .field("Uptime", format_uptime(uptime), true)
                    .field("WS Ping", latency, true)
                    .field("Shard", ctx.shard_id.to_string(), true)
                    .field("Guilds", ctx.cache.guild_count().to_string(), true)
                    .field("Memory (RSS)", format!("{} MB", rss_mb()), true)
                    .field("Guilds in lockdown", locked_count().to_string(), true)
                    .field("Build", concat!("Guardian v", env!("CARGO_PKG_VERSION"), " · Rust"), true)
                    .footer(CreateEmbedFooter::new("Use /nuketest to check my permissions in this server."))
                    .timestamp(Timestamp::now()),
                true,
            )
            .await;
        }

        // ── /tickets ───────────────────────────────────────────
        "tickets" => {
            if !privileged {
                return reply_text(ctx, i, "Only the bot owner or the server owner can set up tickets.").await;
            }
            let cfg = get_ticket_config(&gid);
            match subcmd.as_deref().unwrap_or("") {
                "addtype" => {
                    let key: String = opts
                        .str("key")
                        .unwrap_or("")
                        .trim()
                        .to_lowercase()
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
                        .take(32)
                        .collect();
                    if key.is_empty() {
                        return reply_text(ctx, i, "I don't recognise that key.").await;
                    }
                    let label = truncate(opts.str("label").unwrap_or("").trim(), 80);
                    let emoji = opts.str("emoji").unwrap_or("").trim().to_string();
                    let Some(log_channel) = opts.channel("log_channel") else { return };
                    update_ticket_config(&gid, |c| {
                        c.types.retain(|t| t.key != key);
                        c.types.push(TicketType {
                            key: key.clone(),
                            label: label.clone(),
                            emoji: emoji.clone(),
                            log_channel_id: log_channel.to_string(),
                        });
                    });
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("Ticket type **{label}** (`{key}`) → logs to <#{log_channel}>.\nRun `/tickets panel` to refresh the panel with this type."), Some("Ticket Type Saved")), true).await;
                }
                "removetype" => {
                    let key = opts.str("key").unwrap_or("").trim().to_lowercase();
                    let had = cfg.types.iter().any(|t| t.key == key);
                    update_ticket_config(&gid, |c| c.types.retain(|t| t.key != key));
                    reply_embed(ctx, i, embed(if had { colors::SUCCESS } else { colors::WARN }, if had { format!("Removed ticket type `{key}`. Run `/tickets panel` to refresh the panel.") } else { format!("No ticket type `{key}` was configured.") }, Some("Ticket Type Removed")), true).await;
                }
                "listtypes" => {
                    if cfg.types.is_empty() {
                        return reply_text(ctx, i, "No ticket types yet. Add one with `/tickets addtype`.").await;
                    }
                    let lines = cfg
                        .types
                        .iter()
                        .map(|t| format!("{} **{}** (`{}`) → <#{}>", if t.emoji.is_empty() { "🎫" } else { &t.emoji }, t.label, t.key, t.log_channel_id))
                        .collect::<Vec<_>>()
                        .join("\n");
                    reply_embed(ctx, i, embed(colors::INFO, lines, Some("Ticket Types")), true).await;
                }
                "category" => {
                    let Some(category) = opts.channel("category") else { return };
                    update_ticket_config(&gid, |c| c.category_id = category.to_string());
                    let name = ctx.cache.guild(guild_id).and_then(|g| g.channels.get(&category).map(|c| c.name.to_string())).unwrap_or_default();
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("New tickets will open under **{name}** from now on."), Some("Ticket Category Set")), true).await;
                }
                "panel" => {
                    if cfg.types.is_empty() {
                        return reply_text(ctx, i, "Set up at least one ticket type first with `/tickets addtype`.").await;
                    }
                    let channel = opts
                        .channel("channel")
                        .or_else(|| cfg.panel_channel_id.parse::<u64>().ok().map(ChannelId::new));
                    let Some(channel) = channel else {
                        return reply_text(ctx, i, "Pick a channel, there isn't one set yet.").await;
                    };
                    defer(ctx, i).await;
                    match post_or_edit_panel(ctx, guild_id, channel, &cfg).await {
                        Ok(()) => edit_text(ctx, i, format!("Done - the ticket panel is up in <#{channel}>.")).await,
                        Err(why) => edit_text(ctx, i, why).await,
                    }
                }
                _ => {}
            }
        }

        // ── /applications ──────────────────────────────────────
        "applications" => {
            if !privileged {
                return reply_text(ctx, i, "Only the bot owner or the server owner can set up applications.").await;
            }
            let sub = subcmd.as_deref().unwrap_or("");

            if sub == "list" {
                let apps = get_applications(&gid);
                if apps.is_empty() {
                    return reply_text(ctx, i, "No applications set up yet. They get seeded on first boot when `GUILD_ID` is set.").await;
                }
                let mut e = CreateEmbed::new().color(colors::INFO).title("📝 Applications").timestamp(Timestamp::now());
                for a in apps.values() {
                    e = e.field(
                        format!("{} {} (`{}`) - {}", if a.emoji.is_empty() { "📝" } else { &a.emoji }, a.label, a.key, if a.closed { "🔒 Closed" } else { "🟢 Open" }),
                        format!(
                            "Panel: {} · Review: {}\nRoles on accept: {}\nQuestions: {}",
                            if a.panel_channel_id.is_empty() { "❌ not set".into() } else { format!("<#{}>", a.panel_channel_id) },
                            if a.review_channel_id.is_empty() { "❌ not set".into() } else { format!("<#{}>", a.review_channel_id) },
                            if a.accepted_role_ids.is_empty() { "none".into() } else { a.accepted_role_ids.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ") },
                            a.questions.len()
                        ),
                        false,
                    );
                }
                return reply_embed(ctx, i, e, true).await;
            }

            // open / close accept a key OR the literal "all".
            if sub == "open" || sub == "close" {
                let want_closed = sub == "close";
                let raw_key = opts.str("key").unwrap_or("").trim().to_lowercase();
                defer(ctx, i).await;
                let targets: Vec<_> = if raw_key == "all" {
                    get_applications(&gid).values().cloned().collect()
                } else {
                    get_application(&gid, &raw_key).into_iter().collect()
                };
                if targets.is_empty() {
                    return edit_text(ctx, i, format!("I don't have an application called `{raw_key}`. `/applications list` shows what there is, or use `all`.")).await;
                }
                let mut changed = Vec::new();
                for a in &targets {
                    update_application(&gid, &a.key, |app| app.closed = want_closed);
                    if let Some(fresh) = get_application(&gid, &a.key) {
                        refresh_app_panel(ctx, guild_id, &fresh).await;
                    }
                    changed.push(a.label.clone());
                }
                sec_log(
                    ctx,
                    guild_id,
                    if want_closed { "Applications Closed" } else { "Applications Opened" },
                    &format!("<@{}> {} application(s): {}", i.user.id, if want_closed { "closed" } else { "opened" }, changed.join(", ")),
                    if want_closed { colors::NEUTRAL } else { colors::SUCCESS },
                )
                .await;
                return edit_embed(ctx, i, embed(
                    if want_closed { colors::NEUTRAL } else { colors::SUCCESS },
                    format!("{} **{}** application(s): {}.\nThe panel button{} been updated.", if want_closed { "🔒 Closed" } else { "🟢 Opened" }, changed.len(), changed.join(", "), if changed.len() == 1 { " has" } else { "s have" }),
                    Some("Applications"),
                )).await;
            }

            let key = opts.str("key").unwrap_or("").trim().to_lowercase();

            // `setpanelchannel key:all` collects every application onto one
            // panel. Applications sharing a channel already render as a single
            // embed with a chooser, so pointing them all at one channel is the
            // whole of it.
            if sub == "setpanelchannel" && key == "all" {
                let Some(c) = opts.channel("channel") else { return };
                defer(ctx, i).await;
                let target = c.to_string();
                let apps = get_applications(&gid);
                if apps.is_empty() {
                    return edit_text(ctx, i, "There are no applications to gather up.").await;
                }
                // Old panels in the channels they are leaving would otherwise
                // sit there for ever, still showing buttons.
                crate::systems::applications::retire_panels_outside(ctx, guild_id, &target).await;
                let moved: Vec<String> = apps.values().map(|a| a.label.clone()).collect();
                for k in apps.keys().cloned().collect::<Vec<_>>() {
                    update_application(&gid, &k, |a| {
                        a.panel_channel_id = target.clone();
                        a.panel_message_id.clear();
                    });
                }
                crate::systems::applications::ensure_application_panels(ctx, guild_id).await;
                return edit_embed(ctx, i, embed(
                    colors::SUCCESS,
                    format!(
                        "All **{}** applications are on one panel in <#{c}> now: {}.\nThere's a button for each.",
                        moved.len(),
                        moved.join(", ")
                    ),
                    Some("Applications"),
                )).await;
            }

            let Some(app) = get_application(&gid, &key) else {
                return reply_text(ctx, i, &format!("I don't have an application called `{key}`. `/applications list` shows what there is.")).await;
            };

            match sub {
                "panel" => {
                    let channel_opt = opts.channel("channel");
                    defer(ctx, i).await;
                    if let Some(c) = channel_opt {
                        if c.to_string() != app.panel_channel_id {
                            update_application(&gid, &key, |a| {
                                a.panel_channel_id = c.to_string();
                                a.panel_message_id.clear();
                            });
                        }
                    }
                    let channel_id = channel_opt.map(|c| c.to_string()).unwrap_or(app.panel_channel_id.clone());
                    if channel_id.is_empty() {
                        return edit_text(ctx, i, "Pick a channel, this application hasn't got one yet.").await;
                    }
                    // Render the whole channel group, so a shared channel posts
                    // one combined panel rather than one per app.
                    let group = apps_by_panel_channel(&gid)
                        .into_iter()
                        .find(|(c, _)| *c == channel_id)
                        .map(|(_, a)| a)
                        .unwrap_or_else(|| get_application(&gid, &key).into_iter().collect());
                    render_channel_panel(ctx, guild_id, &channel_id, &group).await;
                    edit_text(ctx, i, format!("Done - the application panel ({}) is up in <#{channel_id}>.", group.iter().map(|a| a.label.clone()).collect::<Vec<_>>().join(", "))).await;
                }
                "setreview" => {
                    let Some(c) = opts.channel("channel") else { return };
                    update_application(&gid, &key, |a| a.review_channel_id = c.to_string());
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("**{}** applications will be sent to <#{c}> for review.", app.label), Some("Applications")), true).await;
                }
                "setoutcome" => {
                    let accepted = opts.channel("accepted");
                    let denied = opts.channel("denied");
                    if accepted.is_none() && denied.is_none() {
                        return reply_text(ctx, i, "Give me an accepted channel, a denied channel, or both.").await;
                    }
                    update_application(&gid, &key, |a| {
                        if let Some(c) = accepted {
                            a.accepted_channel_id = c.to_string();
                        }
                        if let Some(c) = denied {
                            a.denied_channel_id = c.to_string();
                        }
                    });
                    let mut lines = Vec::new();
                    if let Some(c) = accepted {
                        lines.push(format!("Accepted **{}** applications get filed in <#{c}>.", app.label));
                    }
                    if let Some(c) = denied {
                        lines.push(format!("Denied **{}** applications get filed in <#{c}>.", app.label));
                    }
                    reply_embed(ctx, i, embed(colors::SUCCESS, lines.join("\n"), Some("Applications")), true).await;
                }
                "setpanelchannel" => {
                    let Some(c) = opts.channel("channel") else { return };
                    update_application(&gid, &key, |a| {
                        a.panel_channel_id = c.to_string();
                        a.panel_message_id.clear();
                    });
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("**{}** panel channel set to <#{c}>. Run `/applications panel key:{key}` to post it.", app.label), Some("Applications")), true).await;
                }
                "addrole" => {
                    let Some(r) = opts.role("role") else { return };
                    let id = r.to_string();
                    update_application(&gid, &key, |a| {
                        if !a.accepted_role_ids.contains(&id) {
                            a.accepted_role_ids.push(id.clone());
                        }
                    });
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("<@&{r}> will be granted when a **{}** application is accepted.", app.label), Some("Applications")), true).await;
                }
                "removerole" => {
                    let Some(r) = opts.role("role") else { return };
                    let id = r.to_string();
                    update_application(&gid, &key, |a| a.accepted_role_ids.retain(|x| *x != id));
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("<@&{r}> removed from **{}** accepted-roles.", app.label), Some("Applications")), true).await;
                }
                "setquestions" => {
                    let questions: Vec<String> = opts
                        .str("questions")
                        .unwrap_or("")
                        .split('|')
                        .map(|q| q.trim().to_string())
                        .filter(|q| !q.is_empty())
                        .collect();
                    if questions.is_empty() {
                        return reply_text(ctx, i, "Give at least one question, separated by `|`.").await;
                    }
                    update_application(&gid, &key, |a| a.questions = questions.clone());
                    let listed = questions.iter().enumerate().map(|(idx, q)| format!("{}. {q}", idx + 1)).collect::<Vec<_>>().join("\n");
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("**{}** now has **{}** question(s):\n{listed}", app.label, questions.len()), Some("Applications")), true).await;
                }
                _ => {}
            }
        }

        // ── /police manual setup ────────────────────────────────
        "police" => {
            if !privileged {
                return reply_text(ctx, i, "Only the bot owner or the server owner can set up the police manual.").await;
            }
            if group.as_deref() == Some("manual") && subcmd.as_deref() == Some("setup") {
                let channel = opts.channel("channel").unwrap_or(i.channel_id);
                defer(ctx, i).await;
                let posted = channel
                    .send_message(&ctx.http, serenity::builder::CreateMessage::new().embed(build_police_manual_embed()))
                    .await;
                if posted.is_err() {
                    return edit_text(ctx, i, "I couldn't post there. Check that I have permission to send messages and embeds in that channel.").await;
                }
                edit_text(ctx, i, format!("Done - the officer guide & procedures manual is up in <#{channel}>.")).await;
            }
        }

        // ── /chainofcommand ─────────────────────────────────────
        "chainofcommand" => {
            if !privileged {
                return reply_text(ctx, i, "Only the bot owner or the server owner can set up the chain of command.").await;
            }
            let sub = subcmd.as_deref().unwrap_or("");

            if sub == "list" {
                let keys = get_chain_keys(&gid);
                if keys.is_empty() {
                    return reply_text(ctx, i, "No chain-of-command boards set up yet.").await;
                }
                let body = keys
                    .iter()
                    .map(|k| {
                        let c = get_chain(&gid, k);
                        let n: usize = c.groups.iter().map(|g| g.role_ids.len()).sum();
                        format!("`{k}` - {} - {n} role(s)", if c.channel_id.is_empty() { "*(no channel set)*".to_string() } else { format!("<#{}>", c.channel_id) })
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return reply_embed(ctx, i, embed(colors::INFO, body, Some("Chain of Command Boards")), true).await;
            }

            let key = opts.str("key").map(|k| k.trim().to_lowercase()).filter(|k| !k.is_empty()).unwrap_or_else(|| "default".into());

            match sub {
                "setroles" => {
                    let role_ids = extract_ids(opts.str("roles").unwrap_or(""));
                    if role_ids.is_empty() {
                        return reply_text(ctx, i, "Give at least one role, mentioned or by ID.").await;
                    }
                    update_chain(&gid, &key, |b| b.groups = vec![ChainGroup { label: None, role_ids: role_ids.clone() }]);
                    render_chain_of_command(ctx, guild_id, &key).await;
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("Board `{key}` now tracks **{}** role(s), top rank first:\n{}", role_ids.len(), numbered_roles(&role_ids)), Some("Chain of Command")), true).await;
                }
                "setgroup" => {
                    let label = opts.str("label").unwrap_or("").trim().to_string();
                    let role_ids = extract_ids(opts.str("roles").unwrap_or(""));
                    if role_ids.is_empty() {
                        return reply_text(ctx, i, "Give at least one role, mentioned or by ID.").await;
                    }
                    update_chain(&gid, &key, |b| {
                        let existing = b.groups.iter().position(|g| g.label.as_deref().map(|l| l.eq_ignore_ascii_case(&label)).unwrap_or(false));
                        let group = ChainGroup { label: Some(label.clone()), role_ids: role_ids.clone() };
                        match existing {
                            Some(idx) => b.groups[idx] = group,
                            None => b.groups.push(group),
                        }
                    });
                    render_chain_of_command(ctx, guild_id, &key).await;
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("Board `{key}` group **{label}** now tracks **{}** role(s):\n{}", role_ids.len(), numbered_roles(&role_ids)), Some("Chain of Command")), true).await;
                }
                "removegroup" => {
                    let label = opts.str("label").unwrap_or("").trim().to_string();
                    let before = get_chain(&gid, &key).groups.len();
                    update_chain(&gid, &key, |b| {
                        b.groups.retain(|g| !g.label.as_deref().map(|l| l.eq_ignore_ascii_case(&label)).unwrap_or(false))
                    });
                    if get_chain(&gid, &key).groups.len() == before {
                        return reply_text(ctx, i, &format!("Board `{key}` has no group called **{label}**.")).await;
                    }
                    render_chain_of_command(ctx, guild_id, &key).await;
                    reply_embed(ctx, i, embed(colors::SUCCESS, format!("Removed group **{label}** from board `{key}`."), Some("Chain of Command")), true).await;
                }
                "setup" => {
                    let cfg = get_chain(&gid, &key);
                    if cfg.groups.is_empty() {
                        return reply_text(ctx, i, &format!("Board `{key}` has no roles configured yet - run `/chainofcommand setroles` or `setgroup` first.")).await;
                    }
                    let channel = opts.channel("channel").unwrap_or(i.channel_id);
                    let title = opts.str("title").map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
                    defer(ctx, i).await;
                    update_chain(&gid, &key, |b| {
                        if channel.to_string() != b.channel_id {
                            b.channel_id = channel.to_string();
                            b.message_id.clear();
                        }
                        if let Some(t) = &title {
                            b.title = t.clone();
                        }
                    });
                    render_chain_of_command(ctx, guild_id, &key).await;
                    edit_text(ctx, i, format!("Done - board `{key}` is up in <#{channel}>, and will keep itself updated as roles change.")).await;
                }
                "refresh" => {
                    let cfg = get_chain(&gid, &key);
                    if cfg.channel_id.is_empty() || cfg.groups.is_empty() {
                        return reply_text(ctx, i, &format!("Board `{key}` isn't fully configured yet - run `setroles`/`setgroup` and `setup` first.")).await;
                    }
                    defer(ctx, i).await;
                    render_chain_of_command(ctx, guild_id, &key).await;
                    edit_text(ctx, i, "Refreshed.").await;
                }
                "view" => {
                    let cfg = get_chain(&gid, &key);
                    if cfg.groups.is_empty() {
                        return reply_text(ctx, i, &format!("Board `{key}` has no roles configured yet.")).await;
                    }
                    let body = cfg
                        .groups
                        .iter()
                        .map(|g| format!("{}{}", g.label.as_ref().map(|l| format!("**{l}**\n")).unwrap_or_default(), numbered_roles(&g.role_ids)))
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    reply_embed(ctx, i, embed(colors::INFO, format!("Channel: {}\n\n{body}", if cfg.channel_id.is_empty() { "*(not set)*".to_string() } else { format!("<#{}>", cfg.channel_id) }), Some(&format!("Chain of Command - `{key}`"))), true).await;
                }
                _ => {}
            }
        }

        // ── /help ──────────────────────────────────────────────
        "help" => {
            let window_hours = CONFIG.mod_window_ms / 3_600_000;
            let e = CreateEmbed::new()
                .color(colors::INFO)
                .title("🛡️ Guardian Bot - Commands")
                .field("🔇 /mute", "`@user [minutes] [reason]` - Mute (roles stashed & restored on unmute)", false)
                .field("🔊 /unmute", "`@user` - Unmute & restore stashed roles", false)
                .field("👢 /kick", "`@user [reason]` - Kick a member", false)
                .field("🔨 /ban", "`@user [reason] [delete_days]` - Ban a member", false)
                .field("♻️ /unban", "`user_id [reason]` - Unban by ID", false)
                .field("🗑️ /purge", "`count [user]` - Bulk-delete messages", false)
                .field("🔒 /lockdown", "`lock|unlock [channel]` - Lock or unlock a channel", false)
                .field("🚨 /panic", "Emergency lock **all** text channels *(owner only)*", false)
                .field("⚠️ /warn", "`@user [reason]` - Warn (auto-escalates to mute/kick/ban)", false)
                .field("📋 /warnings", "`@user` - View a member's warnings", false)
                .field("🧹 /clearwarns", "`@user` - Clear a member's warnings", false)
                .field("📡 /antiping", "Configure ping protection - `status`, `toggle`, `action`, `protect`, etc. *(bot owner only)*", false)
                .field("📊 /limits", "Check your remaining mod action limits today", false)
                .field("⚙️ /config", "View configuration *(bot owner only)*", false)
                .field("🔧 /setup", "`quick` auto-provisions a mute role + log channels in one step; `view`/`roles`/`channels`/`whitelist`/`failsafe` configure individual fields *(bot/server owner only)*", false)
                .field("🎫 /tickets", "`addtype`/`removetype`/`listtypes`/`category`/`panel` - configure the ticket system *(bot/server owner only)*", false)
                .field("📝 /applications", "`open`/`close` (accepts a key or `all`), `list`/`panel`/`setreview`/`setpanelchannel`/`addrole`/`removerole`/`setquestions` - configure the application system *(bot/server owner only)*", false)
                .field("👮 /police", "`manual setup [channel]` - post the officer guide & procedures manual *(bot/server owner only)*", false)
                .field("📋 /chainofcommand", "`setroles`/`setgroup`/`removegroup`/`setup [channel]`/`refresh`/`view`/`list` - auto-updating role hierarchy boards, each keyed by `key` (defaults to `default`) *(bot/server owner only)*", false)
                .field("🧪 /nuketest", "Confirm anti-nuke + check my permissions *(owner only)*", false)
                .field("📈 /status", "Bot health: uptime, latency, guild count, memory *(owner only)*", false)
                .field("⏱️ Rate Limits", format!("Mod actions are capped over a rolling **{window_hours}h**. `/limits` shows where you are."), false)
                .footer(CreateEmbedFooter::new("Guardian Bot v3 • Security Suite"))
                .timestamp(Timestamp::now());
            reply_embed(ctx, i, e, true).await;
        }

        _ => {}
    }
}

fn opt_channel(id: &str) -> String {
    if id.is_empty() {
        "❌ Not set".into()
    } else {
        format!("<#{id}>")
    }
}
fn opt_role(id: &str) -> String {
    if id.is_empty() {
        "❌ Not set".into()
    } else {
        format!("<@&{id}>")
    }
}
fn id_list(ids: &[String], prefix: &str) -> String {
    if ids.is_empty() {
        "None".into()
    } else {
        ids.iter().map(|id| format!("{prefix}{id}>")).collect::<Vec<_>>().join(", ")
    }
}
fn newline_list(ids: &[String], prefix: &str) -> String {
    if ids.is_empty() {
        "None".into()
    } else {
        ids.iter().map(|id| format!("{prefix}{id}>")).collect::<Vec<_>>().join("\n")
    }
}
fn numbered_roles(ids: &[String]) -> String {
    ids.iter().enumerate().map(|(idx, id)| format!("{}. <@&{id}>", idx + 1)).collect::<Vec<_>>().join("\n")
}

/// Pull every snowflake out of free text (accepts mentions or bare ids),
/// de-duplicated and in order.
fn extract_ids(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let push = |current: &mut String, out: &mut Vec<String>| {
        if current.len() >= 15 && current.len() <= 25 && !out.contains(current) {
            out.push(current.clone());
        }
        current.clear();
    };
    for ch in raw.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else {
            push(&mut current, &mut out);
        }
    }
    push(&mut current, &mut out);
    out
}

/// Resident set size in MB, read from /proc on Linux (0 elsewhere).
fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()))
        .map(|pages| pages * 4096 / 1024 / 1024)
        .unwrap_or(0)
}
