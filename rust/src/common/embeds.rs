//! Embed builders, security logging, and small formatting helpers.

use serenity::builder::{CreateEmbed, CreateMessage};
use serenity::client::Context;
use serenity::model::id::{ChannelId, GuildId};
use serenity::model::Timestamp;

use super::config::{BOT_OWNER_IDS, CONFIG};
use super::db::append_forensic;
use crate::state::guild_settings::gc;

pub mod colors {
    pub const SUCCESS: u32 = 0x00e5a0;
    pub const WARN: u32 = 0xf5a623;
    pub const DANGER: u32 = 0xff3b5c;
    pub const INFO: u32 = 0x5865f2;
    pub const MUTED: u32 = 0xff7518;
    pub const NUKE: u32 = 0xff0033;
    pub const NEUTRAL: u32 = 0x2f3136;
}

/// Appy-style accent colours for the application DM flow and review embed.
pub const APPY_GREEN: u32 = 0x57f287; // intro / submitted / accepted (green left bar)
pub const APPY_BLURPLE: u32 = 0x5865f2; // per-question prompts (blurple left bar)
pub const APPY_RED: u32 = 0xed4245; // denied (red left bar)
pub const APP_PENDING: u32 = 0xf59e0b; // review pending (orange left bar)

/// The standard Guardian embed: coloured bar, description, timestamp, and an
/// optional shield-prefixed title.
pub fn embed(color: u32, description: impl Into<String>, title: Option<&str>) -> CreateEmbed {
    let mut e = CreateEmbed::new().color(color).description(description).timestamp(Timestamp::now());
    if let Some(t) = title {
        e = e.title(format!("🛡️ {t}"));
    }
    e
}

/// Write to the guild's security log channel (and always to the local forensic
/// trail, so a wiped log channel can't erase the record).
pub async fn sec_log(ctx: &Context, guild_id: GuildId, title: &str, desc: &str, color: u32) {
    append_forensic(
        &guild_id.to_string(),
        "log",
        serde_json::json!({ "title": title, "desc": desc }),
    );
    let log_id = gc(&guild_id.to_string()).log_channel_id;
    if log_id.is_empty() {
        return;
    }
    let Ok(id) = log_id.parse::<u64>() else { return };
    let _ = ChannelId::new(id)
        .send_message(&ctx.http, CreateMessage::new().embed(embed(color, desc, Some(title))))
        .await;
}

/// Critical alert: forensic trail + channel ping + owner DM (so a nuked log
/// channel can't blind the owner).
pub async fn alert_owner(ctx: &Context, guild_id: GuildId, desc: &str, color: u32, title: &str) {
    append_forensic(
        &guild_id.to_string(),
        "alert",
        serde_json::json!({ "title": title, "desc": desc }),
    );
    let g = gc(&guild_id.to_string());
    let ch_id = if !g.alert_channel_id.is_empty() { g.alert_channel_id } else { g.log_channel_id };
    let owner_ids: Vec<String> = BOT_OWNER_IDS.iter().cloned().collect();

    if let Ok(id) = ch_id.parse::<u64>() {
        let content = owner_ids.iter().map(|id| format!("<@{id}>")).collect::<Vec<_>>().join(" ");
        let mentions = serenity::builder::CreateAllowedMentions::new()
            .users(owner_ids.iter().filter_map(|s| s.parse::<u64>().ok()).map(serenity::model::id::UserId::new).collect::<Vec<_>>());
        let _ = ChannelId::new(id)
            .send_message(
                &ctx.http,
                CreateMessage::new()
                    .content(content)
                    .embed(embed(color, desc, Some(title)))
                    .allowed_mentions(mentions),
            )
            .await;
    }

    if CONFIG.owner_dm {
        let guild_name = guild_id.name(&ctx.cache).unwrap_or_else(|| guild_id.to_string());
        for id in &owner_ids {
            let Ok(uid) = id.parse::<u64>() else { continue };
            let user_id = serenity::model::id::UserId::new(uid);
            if let Ok(user) = user_id.to_user(&ctx.http).await {
                let _ = user
                    .direct_message(
                        &ctx.http,
                        CreateMessage::new()
                            .embed(embed(color, format!("**[{guild_name}]** {desc}"), Some(title))),
                    )
                    .await;
            }
        }
    }
}

pub fn build_bar(used: usize, limit: usize, width: usize) -> String {
    let denom = limit.max(1) as f64;
    let filled = ((used as f64 / denom) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

pub fn usage_footer(action: &str, used: usize, limit: usize) -> String {
    let remaining = limit.saturating_sub(used);
    let bar = build_bar(used, limit, 10);
    let threshold = (limit as f64 * 0.2).ceil() as usize;
    let warning = if remaining <= threshold && remaining > 0 {
        format!(
            "\nJust **{remaining}** {action}{} remaining today.",
            if remaining == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    format!("`{bar}` **{used}/{limit}** {action}s used today{warning}")
}

pub fn limit_denied_embed(action: &str, used: usize, limit: usize, resets_in_min: i64) -> CreateEmbed {
    embed(
        colors::DANGER,
        format!(
            "You've hit your `/{action}` limit for now.\n\nThat's **{used}/{limit}** {action}s in the last {}h. \
             You'll be able to use it again in about **{resets_in_min} minute{}**.",
            CONFIG.mod_window_ms / 3_600_000,
            if resets_in_min == 1 { "" } else { "s" }
        ),
        None,
    )
}

pub fn render_anti_ping_response(template: &str, member_id: &str, targets: &str, action_text: &str) -> String {
    template
        .replace("{user}", &format!("<@{member_id}>"))
        .replace("{targets}", targets)
        .replace("{action}", action_text)
}

pub fn format_uptime(ms: i64) -> String {
    let s = ms / 1000;
    let d = s / 86_400;
    let h = (s % 86_400) / 3600;
    let m = (s % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m {}s", s % 60)
    }
}
