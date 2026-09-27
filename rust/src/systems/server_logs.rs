//! ProBot-style server logs.
//!
//! Every log type ProBot lists (docs.probot.io/docs/modules/logs) gets its own
//! channel, created by `/setup logs` as `🗃️│<type>-log`, and a ProBot-style
//! embed:
//!   author  → the member the log is about (tag + avatar)
//!   desc    → one bold emoji headline with mentions, then any content
//!   fields  → old/new values, responsible moderator, reason, …
//!   footer  → server name + icon, plus the embed timestamp
//!
//! The embed builders are pure so they can be unit-tested; the event hooks
//! below them do the Discord I/O. Anything with a responsible moderator comes
//! from the audit log, since that is the only place Discord reports who did it.

use once_cell::sync::Lazy;
use serde_json::Value;
use serenity::builder::{CreateChannel, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateMessage};
use serenity::client::Context;
use serenity::model::application::{CommandDataOption, CommandDataOptionValue, CommandInteraction};
use serenity::model::channel::{ChannelType, PermissionOverwrite, PermissionOverwriteType};
use serenity::model::guild::audit_log::{
    Action, AuditLogEntry, Change, ChannelAction, ChannelOverwriteAction, InviteAction, MemberAction, RoleAction,
};
use serenity::model::guild::Member;
use serenity::model::id::{ChannelId, GuildId, RoleId, UserId};
use serenity::model::user::User;
use serenity::model::voice::VoiceState;
use serenity::model::{Permissions, Timestamp};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::state::guild_settings::{gc, update};

const GREEN: u32 = 0x43b581; // created / joined / given / unbanned
const RED: u32 = 0xf04747; // deleted / left / banned / removed
const ORANGE: u32 = 0xfaa61a; // updated / edited
const BLUE: u32 = 0x3498db; // voice + informational

pub const CHANNEL_PREFIX: &str = "🗃️│";

pub struct LogType {
    pub key: &'static str,
    pub label: &'static str,
    pub slug: &'static str,
    pub color: u32,
}

const fn lt(key: &'static str, label: &'static str, slug: &'static str, color: u32) -> LogType {
    LogType { key, label, slug, color }
}

/// In the order ProBot's docs list them, which is also the order `/setup logs`
/// creates the channels in.
pub const LOG_TYPES: [LogType; 27] = [
    // Members
    lt("memberBan", "Member Banned", "ban-log", RED),
    lt("memberUnban", "Member Unbanned", "unban-log", GREEN),
    lt("memberJoin", "Member Joined", "join-log", GREEN),
    lt("memberLeave", "Member Left", "leave-log", RED),
    lt("memberKick", "Member Kicked", "kick-log", RED),
    lt("timeout", "Timeout", "timeout-log", ORANGE),
    // Voice
    lt("voiceJoin", "Member Joined Voice Channel", "voice-join-log", GREEN),
    lt("voiceLeave", "Member Left Voice Channel", "voice-leave-log", RED),
    lt("voiceMove", "Member Moved to Another Voice Channel", "voice-move-log", BLUE),
    lt("voiceDisconnect", "Member Disconnected from Voice Channel", "voice-disconnect-log", RED),
    lt("voiceSwitch", "Member Switched Between Voice Channels", "voice-switch-log", BLUE),
    lt("voiceState", "Voice State", "voice-state-log", BLUE),
    // Channels
    lt("channelCreate", "Channel Created", "channel-create-log", GREEN),
    lt("channelDelete", "Channel Deleted", "channel-delete-log", RED),
    lt("channelUpdate", "Channel Updated", "channel-update-log", ORANGE),
    lt("channelPermissions", "Channel Permissions Updated", "channel-permissions-log", ORANGE),
    // Roles
    lt("roleCreate", "Role Created", "role-create-log", GREEN),
    lt("roleDelete", "Role Deleted", "role-delete-log", RED),
    lt("roleUpdate", "Role Updated", "role-update-log", ORANGE),
    lt("roleGiven", "Role Given", "role-given-log", GREEN),
    lt("roleRemoved", "Role Removed", "role-removed-log", RED),
    // Messages
    lt("messageDelete", "Message Deleted", "message-delete-log", RED),
    lt("messageEdit", "Message Edited", "message-edit-log", ORANGE),
    // Server
    lt("modCommand", "Moderation Command Used", "mod-command-log", BLUE),
    lt("invites", "Server Invites", "invite-log", BLUE),
    lt("serverUpdate", "Update Server", "server-update-log", ORANGE),
    lt("nickname", "Nickname Changed", "nickname-log", ORANGE),
];

fn log_type(key: &str) -> &'static LogType {
    LOG_TYPES.iter().find(|t| t.key == key).expect("unknown log type key")
}

pub fn log_channel_name(t: &LogType) -> String {
    format!("{CHANNEL_PREFIX}{}", t.slug)
}

/// Discord may drop the emoji variation selector, so compare without it.
pub fn same_channel_name(a: &str, b: &str) -> bool {
    a.replace('\u{FE0F}', "") == b.replace('\u{FE0F}', "")
}

// ── Formatting helpers ────────────────────────────────────────

/// The member a log entry is about.
pub struct Who {
    pub id: u64,
    pub tag: String,
    pub avatar: String,
}

impl Who {
    pub fn from_user(u: &User) -> Self {
        Who { id: u.id.get(), tag: u.tag(), avatar: u.face() }
    }
}

/// The server, for the footer.
pub struct Server {
    pub id: u64,
    pub name: String,
    pub icon: Option<String>,
}

fn ts(secs: i64, style: char) -> String {
    format!("<t:{secs}:{style}>")
}

fn full_and_relative(secs: i64) -> String {
    format!("{} ({})", ts(secs, 'F'), ts(secs, 'R'))
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

fn or_none(s: Option<&str>) -> String {
    match s {
        Some(v) if !v.is_empty() => clip(v, 1024),
        _ => "None".to_string(),
    }
}

/// "ManageMessages"-style names for every flag set in `bits`.
fn perm_names(bits: u64) -> Vec<&'static str> {
    Permissions::from_bits_truncate(bits).iter().flat_map(|p| p.get_permission_names()).collect()
}

fn perm_bits(v: &Value) -> u64 {
    match v {
        Value::String(s) => s.parse().unwrap_or(0),
        Value::Number(n) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

fn channel_type_name(kind: u64) -> &'static str {
    match kind {
        0 => "Text",
        2 => "Voice",
        4 => "Category",
        5 => "Announcement",
        13 => "Stage",
        15 => "Forum",
        16 => "Media",
        _ => "Channel",
    }
}

pub fn pretty_key(key: &str) -> String {
    let known = match key {
        "name" => "Name",
        "topic" => "Topic",
        "nsfw" => "NSFW",
        "rate_limit_per_user" => "Slowmode",
        "bitrate" => "Bitrate",
        "user_limit" => "User Limit",
        "position" => "Position",
        "parent_id" => "Category",
        "rtc_region" => "Region",
        "video_quality_mode" => "Video Quality",
        "default_auto_archive_duration" => "Auto Archive",
        "color" => "Color",
        "hoist" => "Displayed Separately",
        "mentionable" => "Mentionable",
        "permissions" => "Permissions",
        "icon_hash" => "Icon",
        "unicode_emoji" => "Emoji",
        "splash_hash" => "Invite Splash",
        "banner_hash" => "Banner",
        "owner_id" => "Owner",
        "afk_channel_id" => "AFK Channel",
        "afk_timeout" => "AFK Timeout",
        "system_channel_id" => "System Channel",
        "rules_channel_id" => "Rules Channel",
        "public_updates_channel_id" => "Updates Channel",
        "verification_level" => "Verification Level",
        "explicit_content_filter" => "Explicit Content Filter",
        "default_message_notifications" => "Default Notifications",
        "mfa_level" => "2FA Requirement",
        "vanity_url_code" => "Vanity URL",
        "description" => "Description",
        "preferred_locale" => "Language",
        "widget_enabled" => "Widget",
        "premium_progress_bar_enabled" => "Boost Progress Bar",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    key.split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn pretty_value(key: &str, v: &Value) -> String {
    let num = v.as_u64();
    match v {
        Value::Null => return "None".to_string(),
        Value::String(s) if s.is_empty() => return "None".to_string(),
        Value::Bool(b) => return if *b { "Yes" } else { "No" }.to_string(),
        _ => {}
    }
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match key {
        "color" => format!("#{:06x}", num.unwrap_or(0)),
        "rate_limit_per_user" | "afk_timeout" => match num {
            Some(0) | None => "Off".to_string(),
            Some(n) => format!("{n}s"),
        },
        "bitrate" => format!("{}kbps", num.unwrap_or(0) / 1000),
        "owner_id" => format!("<@{s}>"),
        k if k.ends_with("_channel_id") || k == "parent_id" => format!("<#{s}>"),
        k if k.ends_with("_hash") => "Changed".to_string(),
        _ => clip(&s, 300),
    }
}

/// One audit-log change as plain JSON: `{ key, old, new }`.
pub struct Diff {
    pub key: String,
    pub old: Value,
    pub new: Value,
}

pub fn diffs(changes: &[Change]) -> Vec<Diff> {
    changes
        .iter()
        .filter(|c| !matches!(c, Change::Unknown))
        .map(|c| {
            let json = serde_json::to_value(c).unwrap_or(Value::Null);
            Diff {
                key: c.key().to_string(),
                old: json.get("old_value").cloned().unwrap_or(Value::Null),
                new: json.get("new_value").cloned().unwrap_or(Value::Null),
            }
        })
        .collect()
}

fn find<'a>(d: &'a [Diff], key: &str) -> Option<&'a Diff> {
    d.iter().find(|x| x.key == key)
}

/// Before/after diff → embed fields, ProBot's "**Old:** / **New:**" pairs.
pub fn change_fields(d: &[Diff]) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    for c in d {
        if matches!(c.key.as_str(), "id" | "type" | "guild_id" | "permission_overwrites" | "flags") {
            continue;
        }
        if c.key == "permissions" {
            let before = perm_names(perm_bits(&c.old));
            let after = perm_names(perm_bits(&c.new));
            let lines: Vec<String> = after
                .iter()
                .filter(|p| !before.contains(p))
                .map(|p| format!("✅ {p}"))
                .chain(before.iter().filter(|p| !after.contains(p)).map(|p| format!("❌ {p}")))
                .collect();
            if !lines.is_empty() {
                out.push(("Permissions".to_string(), clip(&lines.join("\n"), 1024), false));
            }
            continue;
        }
        out.push((
            pretty_key(&c.key),
            format!("**Old:** {}\n**New:** {}", pretty_value(&c.key, &c.old), pretty_value(&c.key, &c.new)),
            true,
        ));
    }
    out.truncate(24); // leave room for the moderator field
    out
}

/// Base ProBot-style embed: author = subject, footer = server, timestamp.
fn base(key: &str, server: &Server, who: Option<&Who>, headline: impl Into<String>) -> CreateEmbed {
    let mut e = CreateEmbed::new()
        .color(log_type(key).color)
        .description(clip(&headline.into(), 4096))
        .timestamp(Timestamp::now());
    if let Some(w) = who {
        e = e.author(CreateEmbedAuthor::new(&w.tag).icon_url(&w.avatar));
    }
    let mut footer = CreateEmbedFooter::new(&server.name);
    if let Some(icon) = &server.icon {
        footer = footer.icon_url(icon);
    }
    e.footer(footer)
}

fn with_mod(e: CreateEmbed, executor: Option<&Who>) -> CreateEmbed {
    match executor {
        Some(m) => e.field("Responsible Moderator", format!("<@{}>", m.id), true),
        None => e,
    }
}

fn with_reason(e: CreateEmbed, reason: Option<&str>, always: bool) -> CreateEmbed {
    match reason {
        Some(r) if !r.is_empty() => e.field("Reason", clip(r, 1024), true),
        _ if always => e.field("Reason", "No reason provided", true),
        _ => e,
    }
}

fn with_fields(mut e: CreateEmbed, fields: Vec<(String, String, bool)>) -> CreateEmbed {
    for (n, v, i) in fields {
        e = e.field(n, v, i);
    }
    e
}

// ── Builders (one per log type) ───────────────────────────────

pub mod build {
    use super::*;

    pub fn member_ban(s: &Server, u: &Who, ex: Option<&Who>, reason: Option<&str>) -> CreateEmbed {
        let e = base("memberBan", s, Some(u), format!("**:airplane: <@{}> banned from the server**", u.id)).thumbnail(&u.avatar);
        with_reason(with_mod(e, ex), reason, true)
    }

    pub fn member_unban(s: &Server, u: &Who, ex: Option<&Who>, reason: Option<&str>) -> CreateEmbed {
        let e = base("memberUnban", s, Some(u), format!("**:unlock: <@{}> unbanned from the server**", u.id)).thumbnail(&u.avatar);
        with_reason(with_mod(e, ex), reason, false)
    }

    pub fn member_join(s: &Server, u: &Who, created_secs: i64, member_count: Option<u64>) -> CreateEmbed {
        let mut e = base("memberJoin", s, Some(u), format!("**:inbox_tray: <@{}> joined the server**", u.id))
            .thumbnail(&u.avatar)
            .field("Account Created", full_and_relative(created_secs), false);
        if let Some(n) = member_count {
            e = e.field("Member Count", n.to_string(), true);
        }
        e
    }

    pub fn member_leave(s: &Server, u: &Who, joined_secs: Option<i64>, roles: &[u64]) -> CreateEmbed {
        let mut e = base("memberLeave", s, Some(u), format!("**:outbox_tray: <@{}> left the server**", u.id)).thumbnail(&u.avatar);
        if let Some(j) = joined_secs {
            e = e.field("Joined", full_and_relative(j), false);
        }
        if !roles.is_empty() {
            let list = roles.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(" ");
            e = e.field(format!("Roles [{}]", roles.len()), clip(&list, 1024), false);
        }
        e
    }

    pub fn member_kick(s: &Server, u: &Who, ex: Option<&Who>, reason: Option<&str>) -> CreateEmbed {
        let e = base("memberKick", s, Some(u), format!("**:boot: <@{}> kicked from the server**", u.id)).thumbnail(&u.avatar);
        with_reason(with_mod(e, ex), reason, true)
    }

    /// `until_secs` = None means the timeout was lifted.
    pub fn timeout(s: &Server, u: &Who, ex: Option<&Who>, reason: Option<&str>, until_secs: Option<i64>) -> CreateEmbed {
        match until_secs {
            Some(until) => {
                let e = base("timeout", s, Some(u), format!("**:stopwatch: <@{}> has been timed out**", u.id))
                    .field("Until", full_and_relative(until), false);
                with_reason(with_mod(e, ex), reason, true)
            }
            None => {
                let e = base("timeout", s, Some(u), format!("**:stopwatch: <@{}> timeout has been removed**", u.id)).color(GREEN);
                with_reason(with_mod(e, ex), reason, false)
            }
        }
    }

    pub fn voice_join(s: &Server, u: &Who, ch: u64) -> CreateEmbed {
        base("voiceJoin", s, Some(u), format!("**:telephone: <@{}> joined voice channel <#{ch}>**", u.id))
    }

    pub fn voice_leave(s: &Server, u: &Who, ch: u64) -> CreateEmbed {
        base("voiceLeave", s, Some(u), format!("**:mute: <@{}> left voice channel <#{ch}>**", u.id))
    }

    pub fn voice_move(s: &Server, u: &Who, ex: Option<&Who>, from: u64, to: u64) -> CreateEmbed {
        with_mod(base("voiceMove", s, Some(u), format!("**:arrow_right_hook: <@{}> moved from <#{from}> to <#{to}>**", u.id)), ex)
    }

    pub fn voice_disconnect(s: &Server, u: &Who, ex: Option<&Who>, ch: u64) -> CreateEmbed {
        with_mod(base("voiceDisconnect", s, Some(u), format!("**:no_entry_sign: <@{}> disconnected from <#{ch}>**", u.id)), ex)
    }

    pub fn voice_switch(s: &Server, u: &Who, from: u64, to: u64) -> CreateEmbed {
        base("voiceSwitch", s, Some(u), format!("**:arrows_counterclockwise: <@{}> switched voice channel <#{from}> ➜ <#{to}>**", u.id))
    }

    /// `changes`: ("Server Mute", true) etc.
    pub fn voice_state(s: &Server, u: &Who, ch: u64, changes: &[(&str, bool)]) -> CreateEmbed {
        let mut e = base("voiceState", s, Some(u), format!("**:microphone2: <@{}> voice state updated in <#{ch}>**", u.id));
        for (what, on) in changes {
            e = e.field(*what, if *on { "✅ On" } else { "❌ Off" }, true);
        }
        e
    }

    pub fn channel_create(s: &Server, ex: Option<&Who>, ch: u64, name: &str, kind: u64) -> CreateEmbed {
        let e = base("channelCreate", s, ex, format!("**:house: {} channel created: <#{ch}>**", channel_type_name(kind)))
            .field("Name", format!("`{name}`"), true);
        with_mod(e, ex)
    }

    pub fn channel_delete(s: &Server, ex: Option<&Who>, name: &str, kind: u64) -> CreateEmbed {
        with_mod(base("channelDelete", s, ex, format!("**:wastebasket: {} channel deleted: `#{name}`**", channel_type_name(kind))), ex)
    }

    pub fn channel_update(s: &Server, ex: Option<&Who>, ch: u64, d: &[Diff]) -> CreateEmbed {
        with_mod(with_fields(base("channelUpdate", s, ex, format!("**:pencil2: Channel updated: <#{ch}>**")), change_fields(d)), ex)
    }

    /// `action`: "create" | "update" | "delete". `bits`: (allow_old, allow_new, deny_old, deny_new).
    pub fn channel_permissions(
        s: &Server,
        ex: Option<&Who>,
        ch: u64,
        target: u64,
        is_role: bool,
        action: &str,
        bits: (u64, u64, u64, u64),
    ) -> CreateEmbed {
        let (a_old, a_new, d_old, d_new) = bits;
        let state = |allow: u64, deny: u64, p: Permissions| {
            if allow & p.bits() != 0 {
                1
            } else if deny & p.bits() != 0 {
                -1
            } else {
                0
            }
        };
        let mut lines = Vec::new();
        for p in Permissions::from_bits_truncate(a_old | a_new | d_old | d_new).iter() {
            let before = state(a_old, d_old, p);
            let after = state(a_new, d_new, p);
            if before == after {
                continue;
            }
            let icon = match after {
                1 => "✅",
                -1 => "❌",
                _ => "⬜",
            };
            for name in p.get_permission_names() {
                lines.push(format!("{icon} {name}"));
            }
        }
        let who = if !is_role {
            format!("<@{target}>")
        } else if target == s.id {
            "@everyone".to_string()
        } else {
            format!("<@&{target}>")
        };
        let verb = match action {
            "create" => "added to",
            "delete" => "removed from",
            _ => "updated for",
        };
        let mut e = base("channelPermissions", s, ex, format!("**:closed_lock_with_key: Channel permissions {verb} <#{ch}>**"))
            .field(if is_role { "Role" } else { "Member" }, who, true);
        e = with_mod(e, ex);
        if !lines.is_empty() {
            e = e.field("Permissions", clip(&lines.join("\n"), 1024), false);
        }
        e
    }

    pub fn role_create(s: &Server, ex: Option<&Who>, role: u64, name: &str) -> CreateEmbed {
        let e = base("roleCreate", s, ex, format!("**:crossed_swords: Role created: <@&{role}>**")).field("Name", format!("`{name}`"), true);
        with_mod(e, ex)
    }

    pub fn role_delete(s: &Server, ex: Option<&Who>, name: &str) -> CreateEmbed {
        with_mod(base("roleDelete", s, ex, format!("**:wastebasket: Role deleted: `{name}`**")), ex)
    }

    pub fn role_update(s: &Server, ex: Option<&Who>, role: u64, d: &[Diff]) -> CreateEmbed {
        with_mod(with_fields(base("roleUpdate", s, ex, format!("**:crossed_swords: Role updated: <@&{role}>**")), change_fields(d)), ex)
    }

    pub fn role_given(s: &Server, u: &Who, ex: Option<&Who>, roles: &[String]) -> CreateEmbed {
        let list = roles.iter().map(|r| format!(":white_check_mark: {r}")).collect::<Vec<_>>().join("\n");
        with_mod(base("roleGiven", s, Some(u), format!("**:writing_hand: <@{}> has been updated.**", u.id)).field("Roles:", clip(&list, 1024), false), ex)
    }

    pub fn role_removed(s: &Server, u: &Who, ex: Option<&Who>, roles: &[String]) -> CreateEmbed {
        let list = roles.iter().map(|r| format!(":no_entry: {r}")).collect::<Vec<_>>().join("\n");
        with_mod(base("roleRemoved", s, Some(u), format!("**:writing_hand: <@{}> has been updated.**", u.id)).field("Roles:", clip(&list, 1024), false), ex)
    }

    pub fn message_delete(s: &Server, u: Option<&Who>, ch: u64, content: &str, attachments: &[String]) -> CreateEmbed {
        let by = u.map(|w| format!("<@{}>", w.id)).unwrap_or_else(|| "an unknown user".to_string());
        let mut e = base("messageDelete", s, u, format!("**:wastebasket: Message sent by {by} deleted in <#{ch}>.**\n{}", clip(content, 3800)));
        if !attachments.is_empty() {
            e = e.field(format!("Attachments ({})", attachments.len()), clip(&attachments.join("\n"), 1024), false);
        }
        e
    }

    pub fn message_bulk_delete(s: &Server, ch: u64, count: usize, lines: &str) -> CreateEmbed {
        let extra = if lines.is_empty() { String::new() } else { format!("\n\n{}", clip(lines, 3800)) };
        base("messageDelete", s, None, format!("**:wastebasket: {count} messages bulk deleted in <#{ch}>.**{extra}"))
    }

    pub fn message_edit(s: &Server, u: &Who, ch: u64, url: &str, before: &str, after: &str) -> CreateEmbed {
        base("messageEdit", s, Some(u), format!("**:pencil: Message edited in <#{ch}>.** [Jump to Message]({url})"))
            .field("Old", or_none(Some(before)).replace("None", "_empty_"), false)
            .field("New", or_none(Some(after)).replace("None", "_empty_"), false)
    }

    pub fn mod_command(s: &Server, u: &Who, ch: u64, command: &str) -> CreateEmbed {
        let name = command.split(' ').next().unwrap_or(command);
        base("modCommand", s, Some(u), format!("**:hammer: <@{}> used `{name}` command in <#{ch}>**\n{}", u.id, clip(command, 3800)))
    }

    pub struct Invite<'a> {
        pub deleted: bool,
        pub code: &'a str,
        pub channel: Option<u64>,
        pub max_uses: u64,
        pub max_age: u64,
        pub temporary: bool,
        pub uses: u64,
    }

    pub fn invites(s: &Server, ex: Option<&Who>, inv: &Invite) -> CreateEmbed {
        let code = inv.code;
        let headline = if inv.deleted {
            format!("**:link: Invite deleted: `discord.gg/{code}`**")
        } else {
            format!("**:link: Invite created: [discord.gg/{code}](https://discord.gg/{code})**")
        };
        let mut e = base("invites", s, ex, headline).color(if inv.deleted { RED } else { GREEN });
        if let Some(c) = inv.channel {
            e = e.field("Channel", format!("<#{c}>"), true);
        }
        if inv.deleted {
            e = e.field("Uses", inv.uses.to_string(), true);
        } else {
            e = e
                .field("Max Uses", if inv.max_uses == 0 { "Unlimited".to_string() } else { inv.max_uses.to_string() }, true)
                .field(
                    "Expires",
                    if inv.max_age == 0 { "Never".to_string() } else { ts(Timestamp::now().unix_timestamp() + inv.max_age as i64, 'R') },
                    true,
                )
                .field("Temporary Membership", if inv.temporary { "Yes" } else { "No" }, true);
        }
        if let Some(m) = ex {
            e = e.field(if inv.deleted { "Deleted By" } else { "Created By" }, format!("<@{}>", m.id), true);
        }
        e
    }

    pub fn server_update(s: &Server, ex: Option<&Who>, d: &[Diff]) -> CreateEmbed {
        with_mod(with_fields(base("serverUpdate", s, ex, "**:gear: Server updated**"), change_fields(d)), ex)
    }

    pub fn nickname(s: &Server, u: &Who, ex: Option<&Who>, before: Option<&str>, after: Option<&str>) -> CreateEmbed {
        let e = base("nickname", s, Some(u), format!("**:writing_hand: <@{}> nickname edited**", u.id))
            .field("Old nickname", or_none(before), true)
            .field("New nickname", or_none(after), true);
        match ex {
            Some(m) if m.id != u.id => with_mod(e, ex),
            _ => e,
        }
    }
}

// ── Routing ───────────────────────────────────────────────────

/// The channel a log type posts to. Deleted/edited messages fall back to the
/// older single message-log channel (`/setup channels msg_log_channel`).
pub fn log_channel_for(guild_id: GuildId, key: &str) -> Option<ChannelId> {
    let g = gc(&guild_id.to_string());
    let id = match g.log_channels.get(key) {
        Some(id) if !id.is_empty() => id.clone(),
        _ if key == "messageDelete" || key == "messageEdit" => g.msg_log_channel_id.clone(),
        _ => return None,
    };
    id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new)
}

/// True for any channel the bot logs into, so it never logs its own logs.
pub fn is_log_channel(guild_id: GuildId, channel_id: ChannelId) -> bool {
    let g = gc(&guild_id.to_string());
    let id = channel_id.to_string();
    g.msg_log_channel_id == id || g.log_channels.values().any(|v| *v == id)
}

pub fn server_of(ctx: &Context, guild_id: GuildId) -> Server {
    match ctx.cache.guild(guild_id) {
        Some(g) => Server { id: guild_id.get(), name: g.name.clone(), icon: g.icon_url() },
        None => Server { id: guild_id.get(), name: guild_id.to_string(), icon: None },
    }
}

pub async fn send(ctx: &Context, guild_id: GuildId, key: &str, embed: CreateEmbed) {
    if let Some(ch) = log_channel_for(guild_id, key) {
        if let Err(e) = ch.send_message(&ctx.http, CreateMessage::new().embed(embed)).await {
            eprintln!("⚠️ [{guild_id}] couldn't post {key} log to channel {ch}: {e}");
        }
    }
}

async fn who(ctx: &Context, id: UserId) -> Option<Who> {
    if id.get() == 0 {
        return None;
    }
    if let Some(u) = ctx.cache.user(id) {
        return Some(Who::from_user(&u));
    }
    id.to_user(&ctx.http).await.ok().map(|u| Who::from_user(&u))
}

// ── Members ──

pub async fn on_member_join(ctx: &Context, member: &Member) {
    if log_channel_for(member.guild_id, "memberJoin").is_none() {
        return;
    }
    let s = server_of(ctx, member.guild_id);
    let count = ctx.cache.guild(member.guild_id).map(|g| g.member_count);
    let e = build::member_join(&s, &Who::from_user(&member.user), member.user.created_at().unix_timestamp(), count);
    send(ctx, member.guild_id, "memberJoin", e).await;
}

pub async fn on_member_leave(ctx: &Context, guild_id: GuildId, user: &User, member: Option<&Member>) {
    if log_channel_for(guild_id, "memberLeave").is_none() {
        return;
    }
    let s = server_of(ctx, guild_id);
    let joined = member.and_then(|m| m.joined_at).map(|t| t.unix_timestamp());
    let roles: Vec<u64> = member.map(|m| m.roles.iter().map(|r| r.get()).collect()).unwrap_or_default();
    send(ctx, guild_id, "memberLeave", build::member_leave(&s, &Who::from_user(user), joined, &roles)).await;
}

// ── Voice ──

/// A mod move/disconnect only shows up in the audit log as an aggregated entry
/// (channel + running count, no target), so a voice change is matched to an
/// entry that is either brand new or whose count just went up.
static VOICE_AUDIT_COUNTS: Lazy<Mutex<HashMap<u64, u64>>> = Lazy::new(|| Mutex::new(HashMap::new()));

async fn voice_moderator(ctx: &Context, guild_id: GuildId, action: MemberAction, channel: Option<ChannelId>) -> Option<Who> {
    // The audit entry lands slightly after the gateway event.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let logs = guild_id.audit_logs(&ctx.http, Some(Action::Member(action)), None, None, Some(5)).await.ok()?;
    let now = Timestamp::now().unix_timestamp();
    let mut executor = None;
    {
        let mut counts = match VOICE_AUDIT_COUNTS.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        for entry in &logs.entries {
            let opts = entry.options.as_ref();
            if let Some(ch) = channel {
                if opts.and_then(|o| o.channel_id) != Some(ch) {
                    continue;
                }
            }
            let count = opts.and_then(|o| o.count).unwrap_or(1);
            let seen = counts.insert(entry.id.get(), count);
            let fresh = match seen {
                None => now - entry.id.created_at().unix_timestamp() < 10,
                Some(prev) => count > prev,
            };
            if fresh {
                executor = Some(entry.user_id);
                break;
            }
        }
    }
    who(ctx, executor?).await
}

pub async fn on_voice_state(ctx: &Context, old: Option<&VoiceState>, new: &VoiceState) {
    let Some(guild_id) = new.guild_id else { return };
    let from = old.and_then(|o| o.channel_id);
    let to = new.channel_id;
    let user = match new.member.as_ref() {
        Some(m) => Some(Who::from_user(&m.user)),
        None => who(ctx, new.user_id).await,
    };
    let Some(user) = user else { return };
    let s = server_of(ctx, guild_id);

    match (from, to) {
        (None, Some(to)) => send(ctx, guild_id, "voiceJoin", build::voice_join(&s, &user, to.get())).await,
        (Some(from), None) => {
            let m = voice_moderator(ctx, guild_id, MemberAction::MemberDisconnect, None).await;
            match m {
                Some(m) if m.id != user.id => {
                    send(ctx, guild_id, "voiceDisconnect", build::voice_disconnect(&s, &user, Some(&m), from.get())).await
                }
                _ => send(ctx, guild_id, "voiceLeave", build::voice_leave(&s, &user, from.get())).await,
            }
        }
        (Some(from), Some(to)) if from != to => {
            let m = voice_moderator(ctx, guild_id, MemberAction::MemberMove, Some(to)).await;
            match m {
                Some(m) if m.id != user.id => {
                    send(ctx, guild_id, "voiceMove", build::voice_move(&s, &user, Some(&m), from.get(), to.get())).await
                }
                _ => send(ctx, guild_id, "voiceSwitch", build::voice_switch(&s, &user, from.get(), to.get())).await,
            }
        }
        (Some(_), Some(to)) => {
            let Some(old) = old else { return };
            let pairs = [
                ("Server Mute", old.mute, new.mute),
                ("Server Deafen", old.deaf, new.deaf),
                ("Self Mute", old.self_mute, new.self_mute),
                ("Self Deafen", old.self_deaf, new.self_deaf),
                ("Stream", old.self_stream.unwrap_or(false), new.self_stream.unwrap_or(false)),
                ("Camera", old.self_video, new.self_video),
            ];
            let changes: Vec<(&str, bool)> = pairs.iter().filter(|(_, a, b)| a != b).map(|(w, _, b)| (*w, *b)).collect();
            if !changes.is_empty() {
                send(ctx, guild_id, "voiceState", build::voice_state(&s, &user, to.get(), &changes)).await;
            }
        }
        (None, None) => {}
    }
}

// ── Audit log (everything with a responsible moderator) ──

fn value_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn role_names(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|r| r.get("name").and_then(|n| n.as_str()).map(str::to_string)).collect())
        .unwrap_or_default()
}

pub async fn on_audit_log_entry(ctx: &Context, entry: &AuditLogEntry, guild_id: GuildId) {
    let d = entry.changes.as_deref().map(diffs).unwrap_or_default();
    let get_new = |k: &str| find(&d, k).map(|c| c.new.clone()).unwrap_or(Value::Null);
    let get_old = |k: &str| find(&d, k).map(|c| c.old.clone()).unwrap_or(Value::Null);
    let target = entry.target_id.map(|t| t.get()).unwrap_or(0);
    let reason = entry.reason.as_deref();

    // Work out which log this is before any HTTP, and skip unconfigured ones.
    let key = match entry.action {
        Action::Member(MemberAction::BanAdd) => "memberBan",
        Action::Member(MemberAction::BanRemove) => "memberUnban",
        Action::Member(MemberAction::Kick) => "memberKick",
        Action::Member(MemberAction::Update) => "memberUpdate",
        Action::Member(MemberAction::RoleUpdate) => "roleUpdateMember",
        Action::Channel(ChannelAction::Create) => "channelCreate",
        Action::Channel(ChannelAction::Delete) => "channelDelete",
        Action::Channel(ChannelAction::Update) => "channelUpdate",
        Action::ChannelOverwrite(_) => "channelPermissions",
        Action::Role(RoleAction::Create) => "roleCreate",
        Action::Role(RoleAction::Delete) => "roleDelete",
        Action::Role(RoleAction::Update) => "roleUpdate",
        Action::Invite(InviteAction::Create) | Action::Invite(InviteAction::Delete) => "invites",
        Action::GuildUpdate => "serverUpdate",
        _ => return,
    };
    let wanted = match key {
        "memberUpdate" => log_channel_for(guild_id, "nickname").is_some() || log_channel_for(guild_id, "timeout").is_some(),
        "roleUpdateMember" => log_channel_for(guild_id, "roleGiven").is_some() || log_channel_for(guild_id, "roleRemoved").is_some(),
        k => log_channel_for(guild_id, k).is_some(),
    };
    if !wanted {
        return;
    }

    let s = server_of(ctx, guild_id);
    let ex = who(ctx, entry.user_id).await;
    let ex = ex.as_ref();
    let target_user = || async { who(ctx, UserId::new(target.max(1))).await };

    match key {
        "memberBan" | "memberUnban" | "memberKick" => {
            let Some(u) = target_user().await else { return };
            let e = match key {
                "memberBan" => build::member_ban(&s, &u, ex, reason),
                "memberUnban" => build::member_unban(&s, &u, ex, reason),
                _ => build::member_kick(&s, &u, ex, reason),
            };
            send(ctx, guild_id, key, e).await;
        }
        "memberUpdate" => {
            let Some(u) = target_user().await else { return };
            if let Some(n) = find(&d, "nick") {
                let e = build::nickname(&s, &u, ex, value_str(&n.old).as_deref(), value_str(&n.new).as_deref());
                send(ctx, guild_id, "nickname", e).await;
            }
            if let Some(t) = find(&d, "communication_disabled_until") {
                let until = value_str(&t.new).and_then(|v| Timestamp::parse(&v).ok()).map(|t| t.unix_timestamp());
                send(ctx, guild_id, "timeout", build::timeout(&s, &u, ex, reason, until)).await;
            }
        }
        "roleUpdateMember" => {
            let Some(u) = target_user().await else { return };
            let added = role_names(&get_new("$add"));
            let removed = role_names(&get_new("$remove"));
            if !added.is_empty() {
                send(ctx, guild_id, "roleGiven", build::role_given(&s, &u, ex, &added)).await;
            }
            if !removed.is_empty() {
                send(ctx, guild_id, "roleRemoved", build::role_removed(&s, &u, ex, &removed)).await;
            }
        }
        "channelCreate" => {
            let name = value_str(&get_new("name")).unwrap_or_default();
            let kind = get_new("type").as_u64().unwrap_or(0);
            send(ctx, guild_id, key, build::channel_create(&s, ex, target, &name, kind)).await;
        }
        "channelDelete" => {
            let name = value_str(&get_old("name")).unwrap_or_default();
            let kind = get_old("type").as_u64().unwrap_or(0);
            send(ctx, guild_id, key, build::channel_delete(&s, ex, &name, kind)).await;
        }
        "channelUpdate" => {
            if d.is_empty() {
                return;
            }
            send(ctx, guild_id, key, build::channel_update(&s, ex, target, &d)).await;
        }
        "channelPermissions" => {
            let Some(opts) = entry.options.as_ref() else { return };
            let Some(ow) = opts.id.map(|i| i.get()) else { return };
            let is_role = opts.kind.as_deref() == Some("0");
            let action = match entry.action {
                Action::ChannelOverwrite(ChannelOverwriteAction::Create) => "create",
                Action::ChannelOverwrite(ChannelOverwriteAction::Delete) => "delete",
                _ => "update",
            };
            let bits = (perm_bits(&get_old("allow")), perm_bits(&get_new("allow")), perm_bits(&get_old("deny")), perm_bits(&get_new("deny")));
            send(ctx, guild_id, key, build::channel_permissions(&s, ex, target, ow, is_role, action, bits)).await;
        }
        "roleCreate" => {
            let name = value_str(&get_new("name")).unwrap_or_default();
            send(ctx, guild_id, key, build::role_create(&s, ex, target, &name)).await;
        }
        "roleDelete" => {
            let name = value_str(&get_old("name")).unwrap_or_default();
            send(ctx, guild_id, key, build::role_delete(&s, ex, &name)).await;
        }
        "roleUpdate" => {
            if d.is_empty() {
                return;
            }
            send(ctx, guild_id, key, build::role_update(&s, ex, target, &d)).await;
        }
        "invites" => {
            let deleted = matches!(entry.action, Action::Invite(InviteAction::Delete));
            let v = |k: &str| if deleted { get_old(k) } else { get_new(k) };
            let code = value_str(&v("code")).unwrap_or_default();
            let inv = build::Invite {
                deleted,
                code: &code,
                channel: value_str(&v("channel_id")).and_then(|c| c.parse().ok()),
                max_uses: v("max_uses").as_u64().unwrap_or(0),
                max_age: v("max_age").as_u64().unwrap_or(0),
                temporary: v("temporary").as_bool().unwrap_or(false),
                uses: v("uses").as_u64().unwrap_or(0),
            };
            send(ctx, guild_id, key, build::invites(&s, ex, &inv)).await;
        }
        "serverUpdate" => {
            if d.is_empty() {
                return;
            }
            send(ctx, guild_id, key, build::server_update(&s, ex, &d)).await;
        }
        _ => {}
    }
}

// ── Moderation commands ──

const MOD_COMMANDS: [&str; 11] =
    ["mute", "unmute", "kick", "ban", "unban", "purge", "lockdown", "panic", "warn", "warnings", "clearwarns"];

fn format_option(o: &CommandDataOption) -> String {
    match &o.value {
        CommandDataOptionValue::SubCommand(opts) | CommandDataOptionValue::SubCommandGroup(opts) => {
            std::iter::once(o.name.clone()).chain(opts.iter().map(format_option)).collect::<Vec<_>>().join(" ")
        }
        CommandDataOptionValue::User(id) => format!("{}:<@{id}>", o.name),
        CommandDataOptionValue::Role(id) => format!("{}:<@&{id}>", o.name),
        CommandDataOptionValue::Channel(id) => format!("{}:<#{id}>", o.name),
        CommandDataOptionValue::Mentionable(id) => format!("{}:{id}", o.name),
        CommandDataOptionValue::String(v) => format!("{}:{v}", o.name),
        CommandDataOptionValue::Integer(v) => format!("{}:{v}", o.name),
        CommandDataOptionValue::Number(v) => format!("{}:{v}", o.name),
        CommandDataOptionValue::Boolean(v) => format!("{}:{v}", o.name),
        _ => o.name.clone(),
    }
}

pub async fn on_command(ctx: &Context, i: &CommandInteraction) {
    let Some(guild_id) = i.guild_id else { return };
    if !MOD_COMMANDS.contains(&i.data.name.as_str()) || log_channel_for(guild_id, "modCommand").is_none() {
        return;
    }
    let command = std::iter::once(format!("/{}", i.data.name))
        .chain(i.data.options.iter().map(format_option))
        .collect::<Vec<_>>()
        .join(" ");
    let s = server_of(ctx, guild_id);
    send(ctx, guild_id, "modCommand", build::mod_command(&s, &Who::from_user(&i.user), i.channel_id.get(), &command)).await;
}

// ── /setup logs ───────────────────────────────────────────────

pub struct LogSetupResult {
    pub created: usize,
    pub reused: usize,
    pub failed: Vec<String>,
    pub category: Option<String>,
}

/// One private `🗃️│<type>-log` channel per log type, under a "Logs" category.
/// Re-running reuses channels that already exist (matched by name) instead of
/// creating duplicates.
pub async fn setup_log_channels(ctx: &Context, guild_id: GuildId, mod_role: Option<RoleId>) -> LogSetupResult {
    let me = ctx.cache.current_user().id;
    let mut overwrites = vec![
        PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::VIEW_CHANNEL,
            kind: PermissionOverwriteType::Role(RoleId::new(guild_id.get())),
        },
        PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS | Permissions::ATTACH_FILES,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Member(me),
        },
    ];
    if let Some(r) = mod_role {
        overwrites.push(PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Role(r),
        });
    }

    // Fetched over HTTP rather than read from cache so a re-run always sees
    // channels created moments ago.
    let existing = guild_id.channels(&ctx.http).await.unwrap_or_default();

    let category = match existing.values().find(|c| c.kind == ChannelType::Category && c.name.eq_ignore_ascii_case("logs")) {
        Some(c) => Some((c.id, c.name.clone())),
        None => guild_id
            .create_channel(
                &ctx.http,
                CreateChannel::new("Logs")
                    .kind(ChannelType::Category)
                    .permissions(overwrites.clone())
                    .audit_log_reason("Guardian /setup logs"),
            )
            .await
            .ok()
            .map(|c| (c.id, c.name)),
    };

    let mut map = gc(&guild_id.to_string()).log_channels;
    let mut result = LogSetupResult { created: 0, reused: 0, failed: Vec::new(), category: category.as_ref().map(|c| c.1.clone()) };
    for t in LOG_TYPES.iter() {
        let name = log_channel_name(t);
        let found = existing.values().find(|c| c.kind == ChannelType::Text && same_channel_name(&c.name, &name)).map(|c| c.id);
        let id = match found {
            Some(id) => {
                result.reused += 1;
                Some(id)
            }
            None => {
                let mut b = CreateChannel::new(&name)
                    .kind(ChannelType::Text)
                    .topic(format!("{} logs", t.label))
                    .permissions(overwrites.clone())
                    .audit_log_reason("Guardian /setup logs");
                if let Some((cat, _)) = &category {
                    b = b.category(*cat);
                }
                match guild_id.create_channel(&ctx.http, b).await {
                    Ok(c) => {
                        result.created += 1;
                        Some(c.id)
                    }
                    Err(e) => {
                        eprintln!("⚠️ /setup logs: couldn't create #{name}: {e}");
                        result.failed.push(name.clone());
                        None
                    }
                }
            }
        };
        if let Some(id) = id {
            map.insert(t.key.to_string(), id.to_string());
        }
    }
    update(&guild_id.to_string(), |s| s.log_channels = map);
    result
}

pub fn build_log_setup_embed(guild_id: GuildId, r: &LogSetupResult) -> CreateEmbed {
    let g = gc(&guild_id.to_string());
    let list = LOG_TYPES
        .iter()
        .map(|t| match g.log_channels.get(t.key) {
            Some(id) => format!("<#{id}> - {}", t.label),
            None => format!("❌ {}", t.label),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut e = CreateEmbed::new()
        .color(if r.failed.is_empty() { crate::common::embeds::colors::SUCCESS } else { crate::common::embeds::colors::WARN })
        .author(serenity::builder::CreateEmbedAuthor::new("🗃️ SERVER LOGS"))
        .title(format!("{}  {} log channels ready", if r.failed.is_empty() { "✅" } else { "⚠️" }, LOG_TYPES.len() - r.failed.len()))
        .description(clip(
            &format!(
                "Every log type now has its own channel{}.\n\n{list}",
                r.category.as_ref().map(|c| format!(" under **{c}**")).unwrap_or_default()
            ),
            4096,
        ))
        .field("🆕 Created", format!("**{}**", r.created), true)
        .field("♻️ Reused", format!("**{}**", r.reused), true)
        .footer(crate::common::theme::footer("Server Logs"))
        .timestamp(Timestamp::now());
    if !r.failed.is_empty() {
        e = e.field(
            "❌ Couldn't create",
            clip(&format!("{}\n_Check I have Manage Channels._", r.failed.join("\n")), 1024),
            false,
        );
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server { id: 1, name: "Test Server".into(), icon: None }
    }
    fn alice() -> Who {
        Who { id: 10, tag: "alice".into(), avatar: "https://cdn/a.png".into() }
    }
    fn modr() -> Who {
        Who { id: 20, tag: "mod".into(), avatar: "https://cdn/m.png".into() }
    }
    fn json(e: CreateEmbed) -> Value {
        serde_json::to_value(e).unwrap()
    }

    #[test]
    fn channel_names_follow_the_prefix_and_are_unique() {
        assert_eq!(LOG_TYPES.len(), 27);
        assert_eq!(log_channel_name(log_type("memberBan")), "🗃️│ban-log");
        let mut slugs: Vec<_> = LOG_TYPES.iter().map(|t| t.slug).collect();
        slugs.sort();
        slugs.dedup();
        assert_eq!(slugs.len(), LOG_TYPES.len());
        let mut keys: Vec<_> = LOG_TYPES.iter().map(|t| t.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), LOG_TYPES.len());
        for t in LOG_TYPES.iter() {
            let n = log_channel_name(t);
            assert!(n.starts_with("🗃️│") && n.ends_with("-log"), "{n}");
            assert!(n.chars().count() <= 100);
        }
        assert!(same_channel_name("🗃│ban-log", "🗃️│ban-log"));
    }

    #[test]
    fn every_log_type_renders_the_probot_layout() {
        let (s, u, m) = (server(), alice(), modr());
        let d = vec![Diff { key: "name".into(), old: "a".into(), new: "b".into() }];
        let embeds: Vec<(&str, CreateEmbed)> = vec![
            ("memberBan", build::member_ban(&s, &u, Some(&m), Some("spam"))),
            ("memberUnban", build::member_unban(&s, &u, Some(&m), None)),
            ("memberJoin", build::member_join(&s, &u, 1_600_000_000, Some(10))),
            ("memberLeave", build::member_leave(&s, &u, Some(1_700_000_000), &[5])),
            ("memberKick", build::member_kick(&s, &u, Some(&m), None)),
            ("timeout", build::timeout(&s, &u, Some(&m), None, Some(1_900_000_000))),
            ("voiceJoin", build::voice_join(&s, &u, 3)),
            ("voiceLeave", build::voice_leave(&s, &u, 3)),
            ("voiceMove", build::voice_move(&s, &u, Some(&m), 3, 4)),
            ("voiceDisconnect", build::voice_disconnect(&s, &u, Some(&m), 3)),
            ("voiceSwitch", build::voice_switch(&s, &u, 3, 4)),
            ("voiceState", build::voice_state(&s, &u, 3, &[("Server Mute", true)])),
            ("channelCreate", build::channel_create(&s, Some(&m), 3, "general", 0)),
            ("channelDelete", build::channel_delete(&s, Some(&m), "general", 0)),
            ("channelUpdate", build::channel_update(&s, Some(&m), 3, &d)),
            ("channelPermissions", build::channel_permissions(&s, Some(&m), 3, 1, true, "update", (0, 1024, 2048, 0))),
            ("roleCreate", build::role_create(&s, Some(&m), 5, "new")),
            ("roleDelete", build::role_delete(&s, Some(&m), "old")),
            ("roleUpdate", build::role_update(&s, Some(&m), 5, &d)),
            ("roleGiven", build::role_given(&s, &u, Some(&m), &["VIP".into()])),
            ("roleRemoved", build::role_removed(&s, &u, Some(&m), &["VIP".into()])),
            ("messageDelete", build::message_delete(&s, Some(&u), 3, "hello", &[])),
            ("messageEdit", build::message_edit(&s, &u, 3, "https://discord.com/x", "a", "b")),
            ("modCommand", build::mod_command(&s, &u, 3, "/ban user:<@2> reason:spam")),
            (
                "invites",
                build::invites(
                    &s,
                    Some(&m),
                    &build::Invite { deleted: false, code: "abc", channel: Some(3), max_uses: 0, max_age: 0, temporary: false, uses: 0 },
                ),
            ),
            ("serverUpdate", build::server_update(&s, Some(&m), &d)),
            ("nickname", build::nickname(&s, &u, Some(&m), None, Some("Ali"))),
        ];
        assert_eq!(embeds.len(), LOG_TYPES.len(), "one sample per log type");
        for (key, e) in embeds {
            let j = json(e);
            let desc = j["description"].as_str().unwrap();
            assert!(desc.starts_with("**"), "{key}: bold headline");
            assert_eq!(j["footer"]["text"], "Test Server", "{key}: server footer");
            assert!(j["timestamp"].is_string(), "{key}: timestamp");
            assert!(j["author"]["name"].is_string(), "{key}: author");
            for f in j["fields"].as_array().cloned().unwrap_or_default() {
                let v = f["value"].as_str().unwrap();
                assert!(!v.is_empty() && v.chars().count() <= 1024, "{key}: field {}", f["name"]);
            }
        }
    }

    #[test]
    fn probot_wording() {
        let (s, u, m) = (server(), alice(), modr());
        assert_eq!(json(build::member_ban(&s, &u, Some(&m), None))["description"], "**:airplane: <@10> banned from the server**");
        assert_eq!(
            json(build::message_delete(&s, Some(&u), 3, "hi", &[]))["description"],
            "**:wastebasket: Message sent by <@10> deleted in <#3>.**\nhi"
        );
        assert_eq!(json(build::role_given(&s, &u, None, &["VIP".into()]))["fields"][0]["value"], ":white_check_mark: VIP");
        let perms = vec![Diff { key: "permissions".into(), old: "0".into(), new: "8".into() }];
        let j = json(build::role_update(&s, Some(&m), 5, &perms));
        assert_eq!(j["fields"][0]["name"], "Permissions");
        assert_eq!(j["fields"][0]["value"], "✅ Administrator");
        let everyone = json(build::channel_permissions(&s, None, 3, 1, true, "update", (0, 1024, 0, 0)));
        assert_eq!(everyone["fields"][0]["value"], "@everyone");
    }

    #[test]
    fn audit_changes_become_old_new_fields() {
        let changes = vec![
            Change::Name { old: Some("a".into()), new: Some("b".into()) },
            Change::RateLimitPerUser { old: Some(0), new: Some(10) },
        ];
        let d = diffs(&changes);
        assert_eq!(d[0].key, "name");
        let f = change_fields(&d);
        assert_eq!(f[0].0, "Name");
        assert_eq!(f[0].1, "**Old:** a\n**New:** b");
        assert_eq!(f[1].0, "Slowmode");
        assert_eq!(f[1].1, "**Old:** Off\n**New:** 10s");
    }
}
