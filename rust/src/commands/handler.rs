//! Slash command dispatch.

use serenity::builder::{
    CreateEmbed, CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage,
    EditInteractionResponse, GetMessages,
};
use serenity::client::Context;
use serenity::model::application::{CommandInteraction, ResolvedOption, ResolvedValue};
use serenity::model::id::{ChannelId, GuildId, MessageId, RoleId, UserId};
use serenity::model::{Permissions, Timestamp};

use crate::common::config::{now_ms, BOT_OWNER_IDS};
use crate::common::embeds::{
    alert_owner, build_bar, colors, embed, format_uptime, limit_denied_embed, render_anti_ping_response, sec_log,
    usage_footer,
};
use crate::common::guildinfo::{channel_in_guild, fetch_member, GuildInfo};
use crate::common::theme::{self, ModAction, Subject};
use crate::common::permissions::{can_act_on, is_guild_admin, is_mod, is_owner, is_whitelisted, try_dm_embed};
use crate::state::anti_ping::{ap, AntiPing};
use crate::state::applications::{get_application, get_applications, update_application};
use crate::state::chain_of_command::{get_chain, get_chain_keys, update_chain, ChainGroup};
use crate::state::guild_settings::{self, gc, update as update_guild};
use crate::state::tunables::{Module, Tunable};
use crate::state::lockdown::{clear_lockdown, is_lockdown, locked_count, lockdown_reason, record_changes, try_set_lockdown};
use crate::state::mod_rates::{check_mod_limit, record_mod_action};
use crate::state::muted_roles::stashed_count;
use crate::state::tickets::{get_ticket_config, update_ticket_config, TicketType};
use crate::state::warnings::{add_warning, clear_warnings, get_warnings};
use crate::systems::anti_nuke::{bump_destructive, event_time, nuke_response, total_reason, Trip, Tripped};
use crate::systems::applications::{apps_by_panel_channel, refresh_app_panel, render_channel_panel};
use crate::systems::chain_of_command::render_chain_of_command;
use crate::systems::mute::{
    lift_lockdown_channels, lock_all_text_channels, mute_user, set_send_messages, unlock_all_text_channels, unmute_user,
};
use crate::systems::police_manual::build_police_manual_embed;
use crate::systems::server_logs::{build_log_setup_embed, setup_log_channels};
use crate::systems::setup_helpers::{build_setup_embed, quick_setup_guild};
use crate::systems::tickets::{post_or_edit_panel, refresh_ticket_panel, types_by_panel_channel};

const STAFF_ONLY: &str = "This one is staff only - you need the mod role.";
const OWNER_ONLY: &str = "This one's owner only.";
/// For commands any server admin can run; see `manager` in `handle`.
const MANAGER_ONLY: &str = "Only the bot owner, the server owner or a server admin can use this one.";

/// Appended when a configuration change was applied in memory but never
/// reached the database. Without it the reply says "saved" and the setting is
/// gone at the next restart, which reads as the bot losing work at random.
const NOT_SAVED: &str = "\n\n\u{274c} **It did not save.** I applied it for now, but writing it to the database failed, so it will be gone the next time I restart. The bot's log has the reason.";

/// `""` when the write went through, the warning above when it did not.
fn save_note(saved: bool) -> &'static str {
    if saved {
        ""
    } else {
        NOT_SAVED
    }
}
/// A chain-of-command board that couldn't be rendered, as a line to append to
/// a reply. The settings themselves are saved either way.
fn render_note(rendered: &Result<(), String>) -> String {
    match rendered {
        Ok(()) => String::new(),
        Err(e) => format!("\n\n⚠️ {e}"),
    }
}
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
    let card = theme::card(theme::Tone::infer(text), None, text);
    reply(ctx, i, CreateInteractionResponseMessage::new().embed(card).ephemeral(true)).await;
}
async fn reply_embed(ctx: &Context, i: &CommandInteraction, e: CreateEmbed, ephemeral: bool) {
    reply(ctx, i, CreateInteractionResponseMessage::new().embed(theme::finish(e)).ephemeral(ephemeral)).await;
}
async fn reply_embeds(ctx: &Context, i: &CommandInteraction, embeds: Vec<CreateEmbed>, ephemeral: bool) {
    reply(ctx, i, CreateInteractionResponseMessage::new().embeds(embeds).ephemeral(ephemeral)).await;
}
async fn defer(ctx: &Context, i: &CommandInteraction) {
    let _ = i
        .create_response(ctx, CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new().ephemeral(true)))
        .await;
}
async fn edit_text(ctx: &Context, i: &CommandInteraction, text: impl Into<String>) {
    let text = text.into();
    let card = theme::card(theme::Tone::infer(&text), None, text);
    let _ = i.edit_response(ctx, EditInteractionResponse::new().content("").embed(card)).await;
}
async fn edit_embed(ctx: &Context, i: &CommandInteraction, e: CreateEmbed) {
    let _ = i.edit_response(ctx, EditInteractionResponse::new().embed(theme::finish(e))).await;
}

/// The avatar and tag of the member a moderation card is about.
fn subject_of(member: &serenity::model::guild::Member) -> (Option<String>, String) {
    (Some(member.user.face()), member.user.tag())
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

/// Pull a `UserId` out of a `/userinfo` argument: a raw id, or a `<@id>` mention.
fn parse_user_arg(raw: &str) -> Option<UserId> {
    extract_ids(raw).first().and_then(|s| s.parse::<u64>().ok()).map(UserId::new)
}

/// Profile card for `/userinfo`: account details for anyone on Discord, plus
/// server-specific details (join date, roles, nickname) when the lookup runs in
/// a server the user is a member of.
async fn userinfo_embed(ctx: &Context, target: UserId, guild_id: Option<GuildId>) -> CreateEmbed {
    let Ok(user) = target.to_user(&ctx.http).await else {
        return theme::card(theme::Tone::Error, Some("User lookup"), format!("I couldn't find a user with the ID `{target}`."));
    };
    let created = target.created_at().unix_timestamp();
    let mut e = CreateEmbed::new()
        .color(theme::palette::AZURE)
        .title(format!("👤  {}", user.tag()))
        .thumbnail(user.face())
        .field("User", format!("<@{}>", user.id), true)
        .field("ID", format!("`{}`", user.id), true)
        .field("Bot", if user.bot { "Yes" } else { "No" }, true)
        .field("Account created", format!("<t:{created}:F> (<t:{created}:R>)"), false)
        .timestamp(Timestamp::now());
    if let Some(gid) = guild_id {
        match fetch_member(ctx, gid, target).await {
            Some(member) => {
                if let Some(nick) = &member.nick {
                    e = e.field("Nickname", nick.clone(), true);
                }
                if let Some(joined) = member.joined_at {
                    let j = joined.unix_timestamp();
                    e = e.field("Joined server", format!("<t:{j}:F> (<t:{j}:R>)"), false);
                }
                let roles: Vec<String> = member.roles.iter().map(|r| format!("<@&{r}>")).collect();
                let value = if roles.is_empty() {
                    "None".to_string()
                } else {
                    let joined = roles.join(" ");
                    joined.chars().take(1024).collect()
                };
                e = e.field(format!("Roles ({})", roles.len()), value, false);
            }
            None => e = e.field("In this server", "Not a member", true),
        }
    }
    e
}

/// The system-status embed, built the same whether `/status` is run in a server
/// or in a bot owner's DMs.
async fn system_status_embed(ctx: &Context) -> CreateEmbed {
    let uptime = crate::START_TIME.get().map(|t| now_ms() - t).unwrap_or(0);
    let latency = crate::shard_latency(ctx.shard_id).await;
    let my_avatar = ctx.cache.current_user().face();
    CreateEmbed::new()
        .color(theme::palette::AZURE)
        .title("📊  GUARDIAN • SYSTEM STATUS")
        .description(match (locked_count(), crate::common::db::write_failures()) {
            (0, 0) => "🟢 **All systems operational.**".to_string(),
            _ => "🟠 **Running, with something that needs a look below.**".to_string(),
        })
        .thumbnail(my_avatar)
        .field("⏱️ Uptime", format!("`{}`", format_uptime(uptime)), true)
        .field("📡 WS Ping", format!("`{latency}`"), true)
        .field("🧩 Shard", format!("`#{}`", ctx.shard_id), true)
        .field("🌐 Guilds", format!("`{}`", ctx.cache.guild_count()), true)
        .field("🧠 Memory", format!("`{} MB`", rss_mb()), true)
        .field(
            "🔒 In lockdown",
            match locked_count() {
                0 => "`none`".to_string(),
                n => format!("**{n}** guild{}", plural(n)),
            },
            true,
        )
        .field(
            "💾 Saving",
            match crate::common::db::write_failures() {
                0 => "🟢 working".to_string(),
                n => format!("🔴 {n} failed write{}", if n == 1 { "" } else { "s" }),
            },
            true,
        )
        .field("🦀 Build", concat!("`v", env!("CARGO_PKG_VERSION"), " · Rust`"), true)
        .footer(theme::footer("Status • /nuketest checks my permissions here"))
        .timestamp(Timestamp::now())
}

pub async fn handle(ctx: &Context, i: &CommandInteraction) {
    // Some commands have no server behind them: a user-installed app runs in
    // DMs, and even in a server the bot may not be a member of it. Handle those
    // here, before anything tries to read guild data.
    let no_guild = i.guild_id.is_none_or(|g| ctx.cache.guild(g).is_none());
    if no_guild {
        match i.data.name.as_str() {
            // Open to anyone, anywhere.
            "help" => {
                let hours = guild_settings::moderation("").window_hours();
                let avatar = Some(ctx.cache.current_user().face());
                return reply_embeds(ctx, i, theme::help_cards(hours, avatar), true).await;
            }
            // Owner-only, and work without a server, so a bot owner can run them
            // straight from the bot's DMs.
            "status" if is_owner(i.user.id) => {
                return reply_embed(ctx, i, system_status_embed(ctx).await, true).await;
            }
            "servers" if is_owner(i.user.id) => {
                defer(ctx, i).await;
                let result = crate::systems::server_list::dm_server_list(ctx, i.user.id).await;
                return edit_text(ctx, i, result).await;
            }
            "status" | "servers" => return reply_text(ctx, i, OWNER_ONLY).await,
            "userinfo" => {
                let raw = dissect(i.data.options()).2.str("user").unwrap_or("").to_string();
                let Some(uid) = parse_user_arg(&raw) else {
                    return reply_text(ctx, i, "Give me a user ID or mention: `/userinfo user:<id>`.").await;
                };
                return reply_embed(ctx, i, userinfo_embed(ctx, uid, None).await, true).await;
            }
            // Everything else acts on a server, which a DM doesn't give it. A
            // bot owner can still reach another server's wipe with `!wipe <id>`.
            _ => {
                return reply_text(
                    ctx,
                    i,
                    "That command works on a server, so run it in the server you mean.",
                )
                .await
            }
        }
    }
    let Some(guild_id) = i.guild_id else {
        return reply_text(ctx, i, "You can only use this in a server.").await;
    };
    // Silence here shows the user "The application did not respond", which says
    // nothing about why. Both of these clear up on their own within moments.
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else {
        return reply_text(ctx, i, "I'm still loading this server's details. Give it a few seconds and try again.").await;
    };
    // The interaction carries the caller's member record; the fetch is only a
    // fallback, so a cache miss can't lock anyone (a bot owner included) out.
    let member = match i.member.as_deref() {
        Some(m) => Some(m.clone()),
        None => fetch_member(ctx, guild_id, i.user.id).await,
    };
    let Some(member) = member else {
        return reply_text(ctx, i, "I couldn't look up your membership in this server just now. Please try again.").await;
    };
    let (group, subcmd, opts) = dissect(i.data.options());
    let gid = guild_id.to_string();
    let nuke = guild_settings::nuke(&gid);
    let modcfg = guild_settings::moderation(&gid);
    let window_hours = modcfg.window_hours();
    let staff = is_mod(&member, info.owner_id);
    let exempt = is_whitelisted(&member, info.owner_id);
    let privileged = is_privileged(i.user.id, info.owner_id);
    // Server management: anyone `privileged`, plus whoever holds Administrator
    // in this server. The anti-nuke controls (/config, /antiraid, /setup
    // whitelist and failsafe) stay on `privileged` alone, because they are what
    // stops a rogue or compromised admin, and an admin who could edit them could
    // simply switch the protection off first.
    let manager = privileged || is_guild_admin(&member, &info);

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
                    return reply_embed(ctx, i, limit_denied_embed("mute", c.used, c.limit, c.resets_in_min, window_hours), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "mute");
            }
            if !mute_user(ctx, &info, &target, minutes, &reason).await {
                return reply_text(ctx, i, NO_MUTE_ROLE).await;
            }
            let c = check_mod_limit(&gid, &i.user.id.to_string(), "mute");
            let stashed = stashed_count(&gid, &target_id.to_string());
            let (avatar, tag) = subject_of(&target);
            let mut e = theme::mod_card(
                ModAction::Mute,
                &Subject { id: target_id.get(), tag: Some(&tag), avatar },
                i.user.id.get(),
                Some(&reason),
                &[
                    ("⏱️ Duration", if minutes > 0 { format!("**{minutes}** minutes") } else { "Until unmuted".to_string() }),
                    ("🎒 Roles set aside", format!("**{stashed}** role{} - handed back on unmute", plural(stashed))),
                ],
            );
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
                theme::mod_card(
                    ModAction::Unmute,
                    &Subject { id: target_id.get(), tag: None, avatar: None },
                    i.user.id.get(),
                    None,
                    &[("🎒 Roles restored", format!("**{stashed}** role{}", plural(stashed)))],
                ),
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
                if let Some(tripped) = nuke_trip(guild_id, i, "kicks", nuke.kick, &nuke) {
                    reply_text(ctx, i, "Hold on - that just tripped the anti-nuke protection.").await;
                    let reason = if tripped.trip == Trip::Category {
                        format!("Issued {}+ kicks via commands in {}s", nuke.kick, nuke.window_ms / 1000)
                    } else {
                        total_reason(&nuke)
                    };
                    return nuke_response(ctx, guild_id, i.user.id, &reason, tripped).await;
                }
                let c = check_mod_limit(&gid, &i.user.id.to_string(), "kick");
                if !c.allowed {
                    return reply_embed(ctx, i, limit_denied_embed("kick", c.used, c.limit, c.resets_in_min, window_hours), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "kick");
            }
            try_dm_embed(&ctx.http, target_id, theme::dm_notice(ModAction::Kick, &info.name, &reason, None)).await;
            if let Err(e) = guild_id.kick_with_reason(&ctx.http, target_id, &reason).await {
                return reply_text(ctx, i, &format!("Discord wouldn't let me kick them: {e}")).await;
            }
            sec_log(
                ctx,
                guild_id,
                "Member Kicked",
                &format!("<@{}> kicked <@{target_id}> - {reason}", i.user.id),
                colors::DANGER,
            )
            .await;
            let c = check_mod_limit(&gid, &i.user.id.to_string(), "kick");
            let (avatar, tag) = subject_of(&target);
            let mut e = theme::mod_card(
                ModAction::Kick,
                &Subject { id: target_id.get(), tag: Some(&tag), avatar },
                i.user.id.get(),
                Some(&reason),
                &[],
            );
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
                if let Some(tripped) = nuke_trip(guild_id, i, "bans", nuke.ban, &nuke) {
                    reply_text(ctx, i, "Hold on - that just tripped the anti-nuke protection.").await;
                    let reason = if tripped.trip == Trip::Category {
                        format!("Issued {}+ bans via commands in {}s", nuke.ban, nuke.window_ms / 1000)
                    } else {
                        total_reason(&nuke)
                    };
                    return nuke_response(ctx, guild_id, i.user.id, &reason, tripped).await;
                }
                let c = check_mod_limit(&gid, &i.user.id.to_string(), "ban");
                if !c.allowed {
                    return reply_embed(ctx, i, limit_denied_embed("ban", c.used, c.limit, c.resets_in_min, window_hours), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "ban");
            }
            try_dm_embed(&ctx.http, target_id, theme::dm_notice(ModAction::Ban, &info.name, &reason, None)).await;
            if let Err(e) = guild_id.ban_with_reason(&ctx.http, target_id, delete_days, &reason).await {
                return reply_text(ctx, i, &format!("Discord wouldn't let me ban them: {e}")).await;
            }
            let c = check_mod_limit(&gid, &i.user.id.to_string(), "ban");
            sec_log(
                ctx,
                guild_id,
                "Member Banned",
                &format!("<@{}> banned <@{target_id}> - {reason}", i.user.id),
                colors::DANGER,
            )
            .await;
            let (avatar, tag) = subject_of(&target);
            let mut extra = Vec::new();
            if delete_days > 0 {
                extra.push(("🧹 Messages removed", format!("Last **{delete_days}** day{}", plural(delete_days as usize))));
            }
            let mut e = theme::mod_card(
                ModAction::Ban,
                &Subject { id: target_id.get(), tag: Some(&tag), avatar },
                i.user.id.get(),
                Some(&reason),
                &extra,
            );
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
            if let Err(e) = guild_id.unban(&ctx.http, uid).await {
                return reply_text(ctx, i, &format!("Discord wouldn't let me lift that ban: {e}")).await;
            }
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
                theme::mod_card(
                    ModAction::Unban,
                    &Subject { id: uid.get(), tag: None, avatar: None },
                    i.user.id.get(),
                    Some(&reason),
                    &[],
                ),
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
                    return reply_embed(ctx, i, limit_denied_embed("purge", c.used, c.limit, c.resets_in_min, window_hours), true).await;
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
                .color(ModAction::Purge.color())
                .author(serenity::builder::CreateEmbedAuthor::new("🗑️ MESSAGES CLEARED"))
                .description(format!("**Cleared {n} message{}{from}.**", plural(n)))
                .field("📍 Channel", format!("<#{}>", i.channel_id), true)
                .field("🛡️ Moderator", format!("<@{}>", i.user.id), true)
                .field("🔢 Requested", format!("**{count}**"), true)
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
            if !channel_in_guild(ctx, guild_id, channel_id) {
                return reply_text(ctx, i, "That channel isn't part of this server.").await;
            }
            if lock && !exempt {
                if let Some(tripped) = nuke_trip(guild_id, i, "chLock", nuke.channel_delete, &nuke) {
                    reply_text(ctx, i, "Hold on - that just tripped the anti-nuke protection.").await;
                    let reason = if tripped.trip == Trip::Category {
                        format!("Locked {}+ channels via commands in {}s", nuke.channel_delete, nuke.window_ms / 1000)
                    } else {
                        total_reason(&nuke)
                    };
                    return nuke_response(ctx, guild_id, i.user.id, &reason, tripped).await;
                }
                let c = check_mod_limit(&gid, &i.user.id.to_string(), "lockdown");
                if !c.allowed {
                    return reply_embed(ctx, i, limit_denied_embed("lockdown", c.used, c.limit, c.resets_in_min, window_hours), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "lockdown");
            }
            // One channel, so fetch just that one rather than the whole list.
            let cached = ctx.cache.guild(guild_id).and_then(|g| g.channels.get(&channel_id).cloned());
            let channel = match cached {
                Some(c) => Some(c),
                None => channel_id.to_channel(&ctx.http).await.ok().and_then(|c| c.guild()),
            };
            let changed = match channel {
                Some(ch) => set_send_messages(ctx, &ch, RoleId::new(guild_id.get()), if lock { Some(false) } else { None }).await,
                None => false,
            };
            if !changed {
                return reply_text(ctx, i, &format!("I couldn't change the permissions on <#{channel_id}>. Check that I have Manage Channels there.")).await;
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
            if !manager {
                return reply_text(ctx, i, MANAGER_ONLY).await;
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
            if try_set_lockdown(&gid, "panic", None).is_none() {
                return edit_text(ctx, i, "A lockdown started at the same moment. Run `/panic` again to lift it.").await;
            }
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
                    return reply_embed(ctx, i, limit_denied_embed("warn", c.used, c.limit, c.resets_in_min, window_hours), true).await;
                }
                record_mod_action(&gid, &i.user.id.to_string(), "warn");
            }
            let total = add_warning(&gid, &target_id.to_string(), &reason, &i.user.id.to_string());
            try_dm_embed(
                &ctx.http,
                target_id,
                theme::dm_notice(
                    ModAction::Warn,
                    &info.name,
                    &reason,
                    Some(&format!("That's warning **#{total}**. More warnings lead to a mute, kick or ban.")),
                ),
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
            if modcfg.warn_ban_at > 0 && total >= modcfg.warn_ban_at {
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
            } else if modcfg.warn_kick_at > 0 && total >= modcfg.warn_kick_at {
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
            } else if modcfg.warn_mute_at > 0 && total >= modcfg.warn_mute_at {
                mute_user(
                    ctx,
                    &info,
                    &target,
                    modcfg.warn_mute_min,
                    &format!("Auto-escalation: reached {total} warnings"),
                )
                .await;
                escalation = format!(
                    "\n🔇 That hit **{total}** warnings, so they've been auto-muted for {} min.",
                    modcfg.warn_mute_min
                );
            }

            let c = check_mod_limit(&gid, &i.user.id.to_string(), "warn");
            let (avatar, tag) = subject_of(&target);
            let mut extra = vec![("🔢 Total warnings", format!("**{total}**"))];
            if !escalation.is_empty() {
                extra.push(("📈 Auto-escalation", escalation.trim().to_string()));
            }
            let mut e = theme::mod_card(
                ModAction::Warn,
                &Subject { id: target_id.get(), tag: Some(&tag), avatar },
                i.user.id.get(),
                Some(&reason),
                &extra,
            );
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
                    .color(theme::palette::ICE)
                    .author(serenity::builder::CreateEmbedAuthor::new(format!("⚠️ WARNING HISTORY • {tag}")))
                    .description(format!("<@{target_id}> has **{} warning{}** on record.\n\n{lines}", list.len(), plural(list.len())))
                    .footer(CreateEmbedFooter::new(format!(
                        "Auto-actions kick in at: mute@{} · kick@{} · ban@{}",
                        modcfg.warn_mute_at, modcfg.warn_kick_at, modcfg.warn_ban_at
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
            if exempt {
                return reply_embed(
                    ctx,
                    i,
                    CreateEmbed::new()
                        .color(theme::palette::SKY)
                        .title("♾️  YOUR MOD LIMITS")
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
                .color(theme::palette::INDIGO)
                .title("📊  YOUR MOD ACTION LIMITS")
                .thumbnail(i.user.face())
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
            if !manager {
                return reply_text(ctx, i, "Only the bot owner, the server owner or a server admin can change these settings.").await;
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
                            .field("Cooldown", format!("{}s", a.cooldown_sec), true)
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
                    if action == "add" && !role_here(&info, role) {
                        return reply_text(ctx, i, "That role isn't part of this server.").await;
                    }
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
                            .field("Exempt channels", newline_list(&a.exempt_channels, "<#"), true)
                            .field("Exempt roles", newline_list(&a.exempt_roles, "<@&"), true)
                            .timestamp(Timestamp::now()),
                        true,
                    )
                    .await;
                }
                "exemptchannel" => {
                    let action = opts.str("action").unwrap_or("add");
                    let Some(ch) = opts.channel("channel") else { return };
                    if action == "add" && !channel_in_guild(ctx, guild_id, ch) {
                        return reply_text(ctx, i, "That channel isn't part of this server.").await;
                    }
                    let id = ch.to_string();
                    let saved = crate::state::anti_ping::update(&gid, |c| {
                        c.exempt_channels.retain(|x| *x != id);
                        if action == "add" {
                            c.exempt_channels.push(id.clone());
                        }
                    });
                    reply_text(ctx, i, &format!("Pings in <#{ch}> {}.{}", if action == "add" { "are no longer policed" } else { "are policed again" }, save_note(saved))).await;
                }
                "exemptrole" => {
                    let action = opts.str("action").unwrap_or("add");
                    let Some(role) = opts.role("role") else { return };
                    if action == "add" && !role_here(&info, role) {
                        return reply_text(ctx, i, "That role isn't part of this server.").await;
                    }
                    let id = role.to_string();
                    let saved = crate::state::anti_ping::update(&gid, |c| {
                        c.exempt_roles.retain(|x| *x != id);
                        if action == "add" {
                            c.exempt_roles.push(id.clone());
                        }
                    });
                    reply_text(ctx, i, &format!("<@&{role}> {} protected targets.{}", if action == "add" { "can now ping" } else { "can no longer ping" }, save_note(saved))).await;
                }
                "cooldown" => {
                    let secs = opts.int("seconds").unwrap_or(10);
                    let saved = crate::state::anti_ping::update(&gid, |c| c.cooldown_sec = secs);
                    reply_text(ctx, i, &format!("After a punishment, the same member won't be punished again for **{secs}s**.{}", save_note(saved))).await;
                }
                _ => {}
            }
        }

        // ── /setup ─────────────────────────────────────────────
        "setup" => {
            if !manager {
                return reply_text(ctx, i, "Only the bot owner, the server owner or a server admin can change these settings.").await;
            }
            if matches!(subcmd.as_deref(), Some("whitelist" | "failsafe")) && !privileged {
                return reply_text(ctx, i, "Only the bot owner or the server owner can change the anti-nuke whitelist and failsafe roles, since they're what stops a rogue admin.").await;
            }
            match subcmd.as_deref().unwrap_or("") {
                "quick" => {
                    defer(ctx, i).await;
                    let mod_role = opts.role("mod_role");
                    let r = quick_setup_guild(ctx, guild_id, mod_role).await;
                    let mut e = build_setup_embed(guild_id, &info.name, &[]);
                    e = e.color(theme::palette::SKY).description(format!(
                        "⚡ **Quick setup finished.**\n{}{}\nNext: `/setup logs` for a channel per log type.",
                        if r.created.is_empty() { String::new() } else { format!("🆕 **Created:** {}\n", r.created.join(", ")) },
                        if r.reused.is_empty() { String::new() } else { format!("♻️ **Reused:** {}\n", r.reused.join(", ")) },
                    ));
                    edit_embed(ctx, i, e).await;
                }
                "logs" => {
                    defer(ctx, i).await;
                    let r = setup_log_channels(ctx, guild_id, opts.role("mod_role")).await;
                    edit_embed(ctx, i, build_log_setup_embed(guild_id, &r)).await;
                }
                "view" => reply_embed(ctx, i, build_setup_embed(guild_id, &info.name, &[]), true).await,
                "roles" => {
                    if [opts.role("mod_role"), opts.role("mute_role")].into_iter().flatten().any(|r| !role_here(&info, r)) {
                        return reply_text(ctx, i, "That role isn't part of this server.").await;
                    }
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
                    let picked = [opts.channel("log_channel"), opts.channel("alert_channel"), opts.channel("msg_log_channel")];
                    if picked.into_iter().flatten().any(|c| !channel_in_guild(ctx, guild_id, c)) {
                        return reply_text(ctx, i, "That channel isn't part of this server.").await;
                    }
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
                    if role.is_some_and(|r| !role_here(&info, r)) {
                        return reply_text(ctx, i, "That role isn't part of this server.").await;
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
                    if !role_here(&info, r) {
                        return reply_text(ctx, i, "That role isn't part of this server.").await;
                    }
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
                return reply_text(ctx, i, "Only the bot owner or the server owner can view or change the config.").await;
            }
            let sub = subcmd.as_deref().unwrap_or("view");
            let module = match sub {
                "antinuke" => Some(Module::AntiNuke),
                "antiraid" => Some(Module::AntiRaid),
                "antispam" => Some(Module::AntiSpam),
                "moderation" => Some(Module::Moderation),
                _ => None,
            };
            if let Some(module) = module {
                let Some(t) = opts.str("setting").and_then(Tunable::from_key).filter(|t| t.module() == module) else {
                    return reply_text(ctx, i, "I don't know that setting.").await;
                };
                let Some(value) = opts.int("value") else {
                    return reply_embed(ctx, i, module_card(&gc(&gid), module), true).await;
                };
                let (lo, hi) = t.range();
                if value < lo || value > hi {
                    let hint = if t.is_bool() { "0 (off) or 1 (on)".to_string() } else { format!("between {lo} and {hi}") };
                    return reply_text(ctx, i, &format!("**{}** has to be {hint}.", t.label())).await;
                }
                let saved = update_guild(&gid, |s| {
                    s.thresholds.insert(t.key().to_string(), value);
                });
                sec_log(
                    ctx,
                    guild_id,
                    "Configuration Changed",
                    &format!("<@{}> set **{}** (`{}`) to **{}**.", i.user.id, t.label(), t.key(), t.format(value)),
                    colors::INFO,
                )
                .await;
                let e = module_card(&gc(&gid), module)
                    .description(format!("**{}** is now **{}** in this server.{}", t.label(), t.format(value), save_note(saved)));
                return reply_embed(ctx, i, e, true).await;
            }
            match sub {
                "reset" => {
                    let raw = opts.str("setting").unwrap_or("").trim().to_lowercase();
                    let saved = if raw == "all" {
                        update_guild(&gid, |s| s.thresholds.clear())
                    } else if let Some(t) = Tunable::from_key(&raw) {
                        update_guild(&gid, |s| {
                            s.thresholds.remove(t.key());
                        })
                    } else {
                        return reply_text(ctx, i, "Give me a setting key like `nuke.ban` (see `/config view`), or `all`.").await;
                    };
                    reply_text(
                        ctx,
                        i,
                        &format!(
                            "Back to the bot-wide default{} for `{raw}` in this server.{}",
                            if raw == "all" { "s" } else { "" },
                            save_note(saved)
                        ),
                    )
                    .await;
                }
                "module" => {
                    let name = opts.str("module").unwrap_or("");
                    let Some(enabled) = opts.boolean("enabled") else { return };
                    let saved = match name {
                        "antinuke" => update_guild(&gid, |s| s.antinuke_disabled = !enabled),
                        "antiraid" => update_guild(&gid, |s| s.antiraid_disabled = !enabled),
                        "antispam" => update_guild(&gid, |s| s.antispam_disabled = !enabled),
                        "antiping" => crate::state::anti_ping::update(&gid, |c| c.enabled = enabled),
                        _ => return reply_text(ctx, i, "I don't know that module.").await,
                    };
                    // Same rule as `/antiraid disable`: a raid lockdown nobody is
                    // watching any more is just a locked server.
                    if name == "antiraid" && !enabled && lockdown_reason(&gid).as_deref() == Some("raid") {
                        lift_lockdown_channels(
                            ctx,
                            guild_id,
                            &format!("<@{}> turned anti-raid off, so I've reopened the channels it locked.", i.user.id),
                        )
                        .await;
                    }
                    let state = if enabled { "on" } else { "off" };
                    sec_log(
                        ctx,
                        guild_id,
                        "Module Toggled",
                        &format!("<@{}> turned **{name}** {state} for this server.", i.user.id),
                        if enabled { colors::SUCCESS } else { colors::WARN },
                    )
                    .await;
                    reply_text(ctx, i, &format!("**{name}** is now **{state}** in this server.{}", save_note(saved))).await;
                }
                _ => {
                    let g = gc(&gid);
                    let a = ap(&gid);
                    let infra = CreateEmbed::new()
                        .color(theme::palette::SAPPHIRE)
                        .title("⚙️  CONFIGURATION • INFRASTRUCTURE")
                        .field("👑 Owner(s)", BOT_OWNER_IDS.iter().map(|id| format!("<@{id}>")).collect::<Vec<_>>().join(", "), false)
                        .field("📜 Log Channel", opt_channel(&g.log_channel_id), true)
                        .field("🚨 Alert Channel", if g.alert_channel_id.is_empty() { "(uses log)".into() } else { format!("<#{}>", g.alert_channel_id) }, true)
                        .field("💬 Msg Log", opt_channel(&g.msg_log_channel_id), true)
                        .field("🔇 Mute Role", opt_role(&g.mute_role_id), true)
                        .field("🛡️ Mod Role", opt_role(&g.mod_role_id), true)
                        .field("🗃️ Server Logs", format!("{}/{} types", g.log_channels.len(), crate::systems::server_logs::LOG_TYPES.len()), true)
                        .field("🏅 Whitelisted Roles", id_list(&g.nuke_whitelist_role_ids, "<@&"), false)
                        .field("🏅 Whitelisted Users", id_list(&g.nuke_whitelist_user_ids, "<@"), false)
                        .field(
                            "📵 Anti-Ping",
                            format!(
                                "{} · action `{}` · {} min\n{} users / {} roles protected",
                                if a.enabled { "🟢 **On**" } else { "🔴 **Off**" },
                                a.action,
                                a.timeout_min,
                                a.protected_users.len(),
                                a.protected_roles.len()
                            ),
                            false,
                        );
                    let mut embeds = vec![infra];
                    for m in [Module::AntiNuke, Module::AntiRaid, Module::AntiSpam, Module::Moderation] {
                        embeds.push(module_card(&g, m));
                    }
                    if let Some(last) = embeds.pop() {
                        embeds.push(
                            last.footer(theme::footer("Config • change with /config <module>, reset with /config reset"))
                                .timestamp(Timestamp::now()),
                        );
                    }
                    reply_embeds(ctx, i, embeds, true).await;
                }
            }
        }

        // ── /nuketest ──────────────────────────────────────────
        "nuketest" => {
            if !manager {
                return reply_text(ctx, i, MANAGER_ONLY).await;
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
                    .color(if need.iter().all(|(_, p)| my_perms.contains(*p)) { theme::palette::SKY } else { theme::palette::ICE })
                    .title("☢️  ANTI-NUKE • SYSTEM CHECK")
                    .description("🟢 **Anti-nuke is armed and watching the audit log.**")
                    .field("🔑 My permissions", status, false)
                    .footer(theme::footer("Anti-Nuke"))
                    .timestamp(Timestamp::now()),
                true,
            )
            .await;
        }

        // ── /status ────────────────────────────────────────────
        "status" => {
            if !manager {
                return reply_text(ctx, i, MANAGER_ONLY).await;
            }
            reply_embed(ctx, i, system_status_embed(ctx).await, true).await;
        }

        // ── /antiraid ──────────────────────────────────────────
        "antiraid" => {
            if !privileged {
                return reply_text(ctx, i, "Only the bot owner or the server owner can change the raid protection.").await;
            }
            let raid = guild_settings::raid(&gid);
            let off = !raid.enabled;
            match subcmd.as_deref().unwrap_or("status") {
                "disable" => {
                    if off {
                        return reply_text(ctx, i, "Anti-raid is already off here.").await;
                    }
                    update_guild(&gid, |g| g.antiraid_disabled = true);

                    // A raid lockdown left standing after the system that set
                    // it has been switched off is just a locked server nobody
                    // is watching, so lift it. Only the raid one: a manual or
                    // panic lock was somebody's decision and stays.
                    let lifted = lockdown_reason(&gid).as_deref() == Some("raid");
                    if lifted {
                        lift_lockdown_channels(
                            ctx,
                            guild_id,
                            &format!("<@{}> turned anti-raid off, so I've reopened the channels it locked.", i.user.id),
                        )
                        .await;
                    }
                    sec_log(
                        ctx,
                        guild_id,
                        "Anti-Raid Disabled",
                        &format!("<@{}> turned the raid protection off for this server.", i.user.id),
                        colors::WARN,
                    )
                    .await;
                    let tail = if lifted {
                        " The raid lockdown that was running is lifted, and the channels it locked are open again."
                    } else {
                        ""
                    };
                    reply_text(
                        ctx,
                        i,
                        &format!(
                            "Anti-raid is off. Joins aren't being counted any more, so nothing will trigger a lockdown or turn away new accounts.{tail} Turn it back on with `/antiraid enable`."
                        ),
                    )
                    .await;
                }
                "enable" => {
                    if !off {
                        return reply_text(ctx, i, "Anti-raid is already on here.").await;
                    }
                    update_guild(&gid, |g| g.antiraid_disabled = false);
                    sec_log(
                        ctx,
                        guild_id,
                        "Anti-Raid Enabled",
                        &format!("<@{}> turned the raid protection back on for this server.", i.user.id),
                        colors::SUCCESS,
                    )
                    .await;
                    reply_text(
                        ctx,
                        i,
                        &format!(
                            "Anti-raid is back on. {} joins inside {} seconds will lock the server down for {} minutes.",
                            raid.join_threshold,
                            raid.window_ms / 1000,
                            raid.lockdown_min
                        ),
                    )
                    .await;
                }
                _ => {
                    let e = CreateEmbed::new()
                        .color(if off { colors::WARN } else { colors::SUCCESS })
                        .title("🚪  ANTI-RAID")
                        .description(if off {
                            "**Off** for this server. Joins aren't being counted, so nothing will trigger a lockdown."
                        } else {
                            "**On** for this server."
                        })
                        .field("Trigger", format!("{} joins in {}s", raid.join_threshold, raid.window_ms / 1000), true)
                        .field("Lockdown length", format!("{} minutes", raid.lockdown_min), true)
                        .field(
                            "New accounts during a lockdown",
                            if raid.kick_new_accounts {
                                format!("turned away under {} minutes old", raid.min_account_age_min)
                            } else {
                                "let through".to_string()
                            },
                            true,
                        )
                        .field("Locked down right now", if is_lockdown(&gid) { "yes" } else { "no" }, true)
                        .footer(CreateEmbedFooter::new(
                            "All of these are this server's own. Change them with /config antiraid.",
                        ));
                    reply_embed(ctx, i, e, true).await;
                }
            }
        }

        // ── /servers ───────────────────────────────────────────
        "servers" => {
            // Bot owner only, not the server owner: this lists every server the
            // bot is in, which is nobody else's business.
            if !is_owner(i.user.id) {
                return reply_text(ctx, i, OWNER_ONLY).await;
            }
            // Creating invites across every server takes well past the three
            // seconds Discord gives an interaction, so defer first.
            defer(ctx, i).await;
            // To the owner who asked. It used to go to one hardcoded account,
            // which meant a deployment with different owners mailed invites to
            // every one of its servers to somebody else.
            let result = crate::systems::server_list::dm_server_list(ctx, i.user.id).await;
            edit_text(ctx, i, result).await;
        }

        // ── /userinfo ──────────────────────────────────────────
        "userinfo" => {
            let raw = opts.str("user").unwrap_or("").to_string();
            let Some(uid) = parse_user_arg(&raw) else {
                return reply_text(ctx, i, "Give me a user ID or mention: `/userinfo user:<id>`.").await;
            };
            reply_embed(ctx, i, userinfo_embed(ctx, uid, Some(guild_id)).await, true).await;
        }

        // ── /tickets ───────────────────────────────────────────
        "tickets" => {
            if !manager {
                return reply_text(ctx, i, "Only the bot owner, the server owner or a server admin can set up tickets.").await;
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
                    let raw_emoji = opts.str("emoji").unwrap_or("").trim().to_string();
                    // Store only what Discord will actually accept on a button.
                    // Keeping an unusable one would break the whole panel later,
                    // a long way from the command that introduced it.
                    let emoji_ok = crate::common::embeds::parse_button_emoji(&raw_emoji).is_some();
                    let emoji = if emoji_ok { raw_emoji.clone() } else { String::new() };
                    let Some(log_channel) = opts.channel("log_channel") else { return };
                    let support_role = opts.role("support_role").map(|r| r.to_string());
                    let type_category = opts.channel("category").map(|c| c.to_string()).unwrap_or_default();
                    // Keep what an existing type already had, so re-running
                    // addtype to fix a label does not silently drop its roles.
                    let existing = cfg.types.iter().find(|t| t.key == key).cloned();
                    let mut support_role_ids = existing.as_ref().map(|t| t.support_role_ids.clone()).unwrap_or_default();
                    if let Some(r) = &support_role {
                        if !support_role_ids.contains(r) {
                            support_role_ids.push(r.clone());
                        }
                    }
                    let category_id = if type_category.is_empty() {
                        existing.as_ref().map(|t| t.category_id.clone()).unwrap_or_default()
                    } else {
                        type_category
                    };
                    let panel_channel_id = existing.as_ref().map(|t| t.panel_channel_id.clone()).unwrap_or_default();
                    let saved = update_ticket_config(&gid, |c| {
                        c.types.retain(|t| t.key != key);
                        c.types.push(TicketType {
                            key: key.clone(),
                            label: label.clone(),
                            emoji: emoji.clone(),
                            log_channel_id: log_channel.to_string(),
                            support_role_ids: support_role_ids.clone(),
                            category_id: category_id.clone(),
                            panel_channel_id: panel_channel_id.clone(),
                            panel_message_id: String::new(),
                        });
                    });
                    refresh_ticket_panel(ctx, guild_id).await;
                    let emoji_note = if raw_emoji.is_empty() || emoji_ok {
                        String::new()
                    } else {
                        format!("\n\n⚠️ I left the emoji off: Discord won't take `{}` on a button. Use an actual emoji, or a custom one from this server as `<:name:id>`.", truncate(&raw_emoji, 40))
                    };
                    let handled_by = if support_role_ids.is_empty() {
                        "the server mod role".to_string()
                    } else {
                        support_role_ids.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ")
                    };
                    let where_ = if category_id.is_empty() {
                        String::new()
                    } else {
                        format!("\nOpens under <#{category_id}>.")
                    };
                    reply_embed(ctx, i, embed(if saved { colors::SUCCESS } else { colors::DANGER }, format!("Ticket type **{label}** (`{key}`) → logs to <#{log_channel}>.\nHandled by {handled_by}.{where_}\nThe panel has been updated with it.{emoji_note}{}", save_note(saved)), Some("Ticket Type Saved")), true).await;
                }
                "removetype" => {
                    let key = opts.str("key").unwrap_or("").trim().to_lowercase();
                    let had = cfg.types.iter().any(|t| t.key == key);
                    let saved = update_ticket_config(&gid, |c| c.types.retain(|t| t.key != key));
                    if had {
                        refresh_ticket_panel(ctx, guild_id).await;
                    }
                    reply_embed(ctx, i, embed(if had && saved { colors::SUCCESS } else if had { colors::DANGER } else { colors::WARN }, if had { format!("Removed ticket type `{key}`. The panel has been updated.{}", save_note(saved)) } else { format!("No ticket type `{key}` was configured.") }, Some("Ticket Type Removed")), true).await;
                }
                "listtypes" => {
                    if cfg.types.is_empty() {
                        return reply_text(ctx, i, "No ticket types yet. Add one with `/tickets addtype`.").await;
                    }
                    let lines = cfg
                        .types
                        .iter()
                        .map(|t| {
                            let roles = if t.support_role_ids.is_empty() {
                                "mod role".to_string()
                            } else {
                                t.support_role_ids.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ")
                            };
                            let cat = if t.category_id.is_empty() { String::new() } else { format!(" · under <#{}>", t.category_id) };
                            let panel = if t.panel_channel_id.is_empty() { String::new() } else { format!(" · panel in <#{}>", t.panel_channel_id) };
                            format!(
                                "{} **{}** (`{}`)\n-# logs to <#{}> · handled by {roles}{cat}{panel}",
                                if t.emoji.is_empty() { "🎫" } else { &t.emoji },
                                t.label,
                                t.key,
                                t.log_channel_id
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    reply_embed(ctx, i, embed(colors::INFO, lines, Some("Ticket Types")), true).await;
                }
                "category" => {
                    let Some(category) = opts.channel("category") else { return };
                    let saved = update_ticket_config(&gid, |c| c.category_id = category.to_string());
                    let name = ctx.cache.guild(guild_id).and_then(|g| g.channels.get(&category).map(|c| c.name.to_string())).unwrap_or_default();
                    reply_embed(ctx, i, embed(if saved { colors::SUCCESS } else { colors::DANGER }, format!("New tickets will open under **{name}** from now on.{}", save_note(saved)), Some("Ticket Category Set")), true).await;
                }
                "support" => {
                    let key = opts.str("key").unwrap_or("").trim().to_lowercase();
                    let add = opts.str("action").unwrap_or("add").eq_ignore_ascii_case("add");
                    let Some(role) = opts.role("role") else { return };
                    if !cfg.types.iter().any(|t| t.key == key) {
                        return reply_text(ctx, i, &format!("There's no ticket type `{key}`. `/tickets listtypes` shows them.")).await;
                    }
                    let rid = role.to_string();
                    let saved = update_ticket_config(&gid, |c| {
                        if let Some(t) = c.types.iter_mut().find(|t| t.key == key) {
                            t.support_role_ids.retain(|r| *r != rid);
                            if add {
                                t.support_role_ids.push(rid.clone());
                            }
                        }
                    });
                    let now = get_ticket_config(&gid).types.iter().find(|t| t.key == key).map(|t| t.support_role_ids.clone()).unwrap_or_default();
                    let listed = if now.is_empty() {
                        "nobody in particular, so it falls back to the server mod role".to_string()
                    } else {
                        now.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ")
                    };
                    reply_embed(ctx, i, embed(if saved { colors::SUCCESS } else { colors::DANGER }, format!("{} <@&{rid}> {} `{key}`.\n\n`{key}` is now handled by {listed}.\n\nThey can see these tickets, reply, claim and close them, and get pinged when one opens. Tickets already open keep the permissions they were created with.{}", if add { "Added" } else { "Removed" }, if add { "to" } else { "from" }, save_note(saved)), Some("Ticket Support Roles")), true).await;
                }
                "typecategory" => {
                    let key = opts.str("key").unwrap_or("").trim().to_lowercase();
                    let Some(category) = opts.channel("category") else { return };
                    if !cfg.types.iter().any(|t| t.key == key) {
                        return reply_text(ctx, i, &format!("There's no ticket type `{key}`. `/tickets listtypes` shows them.")).await;
                    }
                    let saved = update_ticket_config(&gid, |c| {
                        if let Some(t) = c.types.iter_mut().find(|t| t.key == key) {
                            t.category_id = category.to_string();
                        }
                    });
                    reply_embed(ctx, i, embed(if saved { colors::SUCCESS } else { colors::DANGER }, format!("New `{key}` tickets will open under <#{category}>.{}", save_note(saved)), Some("Ticket Category Set")), true).await;
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

                    // With a key, this puts one type on its own panel, the way
                    // an Appy panel is linked to particular templates. Without
                    // one, it moves every type that has not been split out,
                    // which is the single-panel behaviour this always had.
                    let key = opts.str("key").unwrap_or("").trim().to_lowercase();
                    if !key.is_empty() {
                        if !cfg.types.iter().any(|t| t.key == key) {
                            return edit_text(ctx, i, format!("There's no ticket type `{key}`. `/tickets listtypes` shows them.")).await;
                        }
                        update_ticket_config(&gid, |c| {
                            if let Some(t) = c.types.iter_mut().find(|t| t.key == key) {
                                t.panel_channel_id = channel.to_string();
                                // A different channel means a different panel
                                // message, so the old id must not be reused.
                                t.panel_message_id.clear();
                            }
                        });
                    } else {
                        update_ticket_config(&gid, |c| {
                            if c.panel_channel_id != channel.to_string() {
                                c.panel_channel_id = channel.to_string();
                                c.panel_message_id.clear();
                                for t in c.types.iter_mut().filter(|t| t.panel_channel_id.is_empty()) {
                                    t.panel_message_id.clear();
                                }
                            }
                        });
                    }

                    let group = types_by_panel_channel(&gid)
                        .into_iter()
                        .find(|(c, _)| *c == channel.to_string())
                        .map(|(_, t)| t)
                        .unwrap_or_default();
                    match post_or_edit_panel(ctx, guild_id, channel, &group).await {
                        Ok(()) => {
                            let names = group.iter().map(|t| t.label.clone()).collect::<Vec<_>>().join(", ");
                            edit_text(ctx, i, format!("Done - the ticket panel is up in <#{channel}> with: {names}.")).await
                        }
                        Err(why) => edit_text(ctx, i, why).await,
                    }
                }
                _ => {}
            }
        }

        // ── /applications ──────────────────────────────────────
        "applications" => {
            if !manager {
                return reply_text(ctx, i, "Only the bot owner, the server owner or a server admin can set up applications.").await;
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
            if !manager {
                return reply_text(ctx, i, "Only the bot owner, the server owner or a server admin can set up the police manual.").await;
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
            if !manager {
                return reply_text(ctx, i, "Only the bot owner, the server owner or a server admin can set up the chain of command.").await;
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
                    // Free text, so every id is checked against this server's
                    // roles before it is stored.
                    let (role_ids, foreign): (Vec<String>, Vec<String>) = extract_ids(opts.str("roles").unwrap_or(""))
                        .into_iter()
                        .partition(|id| id.parse::<u64>().is_ok_and(|r| role_here(&info, RoleId::new(r))));
                    if !foreign.is_empty() {
                        return reply_text(ctx, i, &format!("These aren't roles in this server: {}", foreign.iter().map(|r| format!("`{r}`")).collect::<Vec<_>>().join(", "))).await;
                    }
                    if role_ids.is_empty() {
                        return reply_text(ctx, i, "Give at least one role, mentioned or by ID.").await;
                    }
                    let saved = update_chain(&gid, &key, |b| b.groups = vec![ChainGroup { label: None, role_ids: role_ids.clone() }]);
                    let rendered = render_chain_of_command(ctx, guild_id, &key).await;
                    reply_embed(ctx, i, embed(if saved { colors::SUCCESS } else { colors::DANGER }, format!("Board `{key}` now tracks **{}** role(s), top rank first:\n{}{}{}", role_ids.len(), numbered_roles(&role_ids), save_note(saved), render_note(&rendered)), Some("Chain of Command")), true).await;
                }
                "setgroup" => {
                    let label = opts.str("label").unwrap_or("").trim().to_string();
                    // Free text, so every id is checked against this server's
                    // roles before it is stored.
                    let (role_ids, foreign): (Vec<String>, Vec<String>) = extract_ids(opts.str("roles").unwrap_or(""))
                        .into_iter()
                        .partition(|id| id.parse::<u64>().is_ok_and(|r| role_here(&info, RoleId::new(r))));
                    if !foreign.is_empty() {
                        return reply_text(ctx, i, &format!("These aren't roles in this server: {}", foreign.iter().map(|r| format!("`{r}`")).collect::<Vec<_>>().join(", "))).await;
                    }
                    if role_ids.is_empty() {
                        return reply_text(ctx, i, "Give at least one role, mentioned or by ID.").await;
                    }
                    let saved = update_chain(&gid, &key, |b| {
                        let existing = b.groups.iter().position(|g| g.label.as_deref().map(|l| l.eq_ignore_ascii_case(&label)).unwrap_or(false));
                        let group = ChainGroup { label: Some(label.clone()), role_ids: role_ids.clone() };
                        match existing {
                            Some(idx) => b.groups[idx] = group,
                            None => b.groups.push(group),
                        }
                    });
                    let rendered = render_chain_of_command(ctx, guild_id, &key).await;
                    reply_embed(ctx, i, embed(if saved { colors::SUCCESS } else { colors::DANGER }, format!("Board `{key}` group **{label}** now tracks **{}** role(s):\n{}{}{}", role_ids.len(), numbered_roles(&role_ids), save_note(saved), render_note(&rendered)), Some("Chain of Command")), true).await;
                }
                "removegroup" => {
                    let label = opts.str("label").unwrap_or("").trim().to_string();
                    let before = get_chain(&gid, &key).groups.len();
                    let saved = update_chain(&gid, &key, |b| {
                        b.groups.retain(|g| !g.label.as_deref().map(|l| l.eq_ignore_ascii_case(&label)).unwrap_or(false))
                    });
                    if get_chain(&gid, &key).groups.len() == before {
                        return reply_text(ctx, i, &format!("Board `{key}` has no group called **{label}**.")).await;
                    }
                    let rendered = render_chain_of_command(ctx, guild_id, &key).await;
                    reply_embed(ctx, i, embed(if saved { colors::SUCCESS } else { colors::DANGER }, format!("Removed group **{label}** from board `{key}`.{}{}", save_note(saved), render_note(&rendered)), Some("Chain of Command")), true).await;
                }
                "setup" => {
                    let cfg = get_chain(&gid, &key);
                    if cfg.groups.is_empty() {
                        return reply_text(ctx, i, &format!("Board `{key}` has no roles configured yet - run `/chainofcommand setroles` or `setgroup` first.")).await;
                    }
                    // Without `channel:`, the board stays where it is. Re-running
                    // setup to retitle or re-post it used to drag it into
                    // whichever channel the command was typed in, leaving the
                    // real board behind, frozen.
                    let current = cfg.channel_id.parse::<u64>().ok().map(ChannelId::new);
                    let channel = opts.channel("channel").or(current).unwrap_or(i.channel_id);
                    let title = opts.str("title").map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
                    defer(ctx, i).await;
                    // The board being replaced, when this moves it to another channel.
                    let moved_from = current
                        .filter(|c| *c != channel)
                        .zip(cfg.message_id.parse::<u64>().ok().map(MessageId::new));
                    let saved = update_chain(&gid, &key, |b| {
                        if channel.to_string() != b.channel_id {
                            b.channel_id = channel.to_string();
                            b.message_id.clear();
                        }
                        if let Some(t) = &title {
                            b.title = t.clone();
                        }
                    });
                    let rendered = render_chain_of_command(ctx, guild_id, &key).await;
                    // Once the new board is up, take the old one down, or it sits
                    // there stale while its footer claims it updates itself.
                    if rendered.is_ok() {
                        if let Some((old_channel, old_message)) = moved_from {
                            let _ = old_channel.delete_message(&ctx.http, old_message).await;
                        }
                    }
                    match rendered {
                        Ok(()) => edit_text(ctx, i, format!("Done - board `{key}` is up in <#{channel}>, and will keep itself updated as roles change.{}", save_note(saved))).await,
                        Err(e) => edit_text(ctx, i, format!("⚠️ Board `{key}` is set to <#{channel}>, but {e}{}", save_note(saved))).await,
                    }
                }
                "refresh" => {
                    let cfg = get_chain(&gid, &key);
                    if cfg.channel_id.is_empty() || cfg.groups.is_empty() {
                        return reply_text(ctx, i, &format!("Board `{key}` isn't fully configured yet - run `setroles`/`setgroup` and `setup` first.")).await;
                    }
                    defer(ctx, i).await;
                    match render_chain_of_command(ctx, guild_id, &key).await {
                        Ok(()) => edit_text(ctx, i, "Refreshed.").await,
                        Err(e) => edit_text(ctx, i, format!("⚠️ {e}")).await,
                    }
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
            let avatar = { Some(ctx.cache.current_user().face()) };
            reply_embeds(ctx, i, theme::help_cards(window_hours, avatar), true).await;
        }

        _ => {}
    }
}

/// Anti-nuke's counters, when anti-nuke is on in this guild.
fn nuke_trip(
    guild_id: GuildId,
    i: &CommandInteraction,
    key: &'static str,
    threshold: usize,
    cfg: &crate::state::tunables::NukeConfig,
) -> Option<Tripped> {
    if !cfg.enabled {
        return None;
    }
    bump_destructive(guild_id, i.user.id, key, threshold, cfg, event_time(i.id.get()))
}

/// One module's thresholds as this guild sees them, marking which are its own
/// and which are the bot-wide default.
fn module_card(g: &crate::state::guild_settings::GuildSettings, module: Module) -> CreateEmbed {
    let (color, icon, on) = match module {
        Module::AntiNuke => (theme::palette::NAVY, "☢️", Some(!g.antinuke_disabled)),
        Module::AntiRaid => (theme::palette::COBALT, "🚪", Some(!g.antiraid_disabled)),
        Module::AntiSpam => (theme::palette::ICE, "🧹", Some(!g.antispam_disabled)),
        Module::Moderation => (theme::palette::INDIGO, "🔨", None),
    };
    let lines = Tunable::ALL
        .iter()
        .filter(|t| t.module() == module)
        .map(|t| {
            let v = crate::state::tunables::resolve(&g.thresholds, *t);
            let own = if g.thresholds.contains_key(t.key()) { " ✏️" } else { "" };
            format!("`{}` {} → **{}**{own}", t.key(), t.label(), t.format(v))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let status = match on {
        Some(true) => "🟢 **On**\n",
        Some(false) => "🔴 **Off**\n",
        None => "",
    };
    CreateEmbed::new()
        .color(color)
        .title(format!("{icon}  CONFIGURATION • {}", module.label().to_uppercase()))
        .description(format!("{status}{lines}\n-# ✏️ = set for this server; the rest are bot-wide defaults."))
}

/// Whether a role id belongs to this guild. Slash-command options are
/// resolved by Discord, but settings are checked here as well before they are
/// saved, so nothing foreign can end up stored against this server.
fn role_here(info: &GuildInfo, role: RoleId) -> bool {
    info.roles.contains_key(&role)
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
