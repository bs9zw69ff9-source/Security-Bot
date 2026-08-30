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

/// Whether a panel message posted earlier is still there.
///
/// The distinction that matters is "Discord says this message does not exist"
/// versus "I could not find out". Only the first justifies posting a
/// replacement. Treating every error as deletion means a missing Read Message
/// History permission, a rate limit, or a brief Discord hiccup all produce a
/// duplicate panel on boot, and because the new id is then saved over the old
/// one, every restart adds another.
///
/// Anything other than a 404 is reported as still present, so the worst case
/// is a panel that does not get refreshed until the next restart, rather than
/// a channel filling up with copies.
pub async fn message_still_exists(
    ctx: &Context,
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
) -> bool {
    match channel_id.message(&ctx.http, message_id).await {
        Ok(_) => true,
        Err(e) if is_unknown_message(&e) => false,
        Err(e) => {
            eprintln!("⚠️ couldn't check whether message {message_id} still exists ({e}); assuming it does, so as not to post a duplicate");
            true
        }
    }
}

/// True only when Discord itself answered "no such message" (404).
///
/// Every other failure - a missing Read Message History permission, a rate
/// limit, an outage, a dropped connection - means the answer is unknown, not
/// that the message is gone.
pub fn is_unknown_message(err: &serenity::prelude::SerenityError) -> bool {
    use serenity::http::HttpError;
    use serenity::prelude::SerenityError;
    matches!(
        err,
        SerenityError::Http(HttpError::UnsuccessfulRequest(res))
            if res.status_code == serenity::http::StatusCode::NOT_FOUND
    )
}

/// Delete the bot's own duplicate panels in a channel, keeping `keep`.
///
/// A panel is recognised by the custom id on its own buttons, so this can only
/// ever match something this bot posted as a panel. Anything else in the
/// channel, from anyone including this bot, is left alone: wiping the channel
/// outright would take staff conversation and pins with it, on every restart,
/// with no way back.
///
/// Scans the most recent 100 messages, which is where a panel lives in
/// practice, and returns how many it removed.
pub async fn remove_duplicate_panels(
    ctx: &Context,
    channel_id: serenity::model::id::ChannelId,
    keep: Option<serenity::model::id::MessageId>,
    marker: &str,
    what: &str,
) -> usize {
    use serenity::builder::GetMessages;

    let me = ctx.cache.current_user().id;
    let Ok(messages) = channel_id.messages(&ctx.http, GetMessages::new().limit(100)).await else {
        return 0;
    };

    let mut removed = 0;
    for msg in messages {
        if msg.author.id != me || Some(msg.id) == keep {
            continue;
        }
        // Only our panels carry these custom ids on their components.
        let is_panel = msg.components.iter().any(|row| {
            serde_json::to_string(row).map(|j| j.contains(marker)).unwrap_or(false)
        });
        if !is_panel {
            continue;
        }
        if msg.delete(&ctx.http).await.is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        println!("🧹 Cleared {removed} stale {what} panel(s) in #{channel_id}");
    }
    removed
}

/// Which of the permissions needed to post a panel the bot is missing in a
/// channel, as human-readable names.
///
/// Checked before posting so the failure can name the problem. "Check my
/// permissions" is not much help when the whole question is which one.
pub fn missing_panel_permissions(
    ctx: &Context,
    guild_id: serenity::model::id::GuildId,
    channel_id: serenity::model::id::ChannelId,
) -> Vec<&'static str> {
    use serenity::model::Permissions;

    let me = ctx.cache.current_user().id;
    let Some(guild) = ctx.cache.guild(guild_id) else { return Vec::new() };
    let Some(channel) = guild.channels.get(&channel_id) else {
        return vec!["a channel in this server (that one isn't in it)"];
    };
    let Some(member) = guild.members.get(&me) else { return Vec::new() };
    let perms = guild.user_permissions_in(channel, member);

    [
        (Permissions::VIEW_CHANNEL, "View Channel"),
        (Permissions::SEND_MESSAGES, "Send Messages"),
        (Permissions::EMBED_LINKS, "Embed Links"),
    ]
    .into_iter()
    .filter(|(p, _)| !perms.contains(*p))
    .map(|(_, name)| name)
    .collect()
}

/// Parse a configured emoji for use on a button, or `None` if Discord would
/// refuse it.
///
/// Serenity's own parser accepts anything that does not start with `<` as a
/// unicode emoji, so `:police:`, a bare word, or a typo all parse happily and
/// are then rejected by the API with "Invalid Form Body ... Invalid emoji".
/// Because one button takes the whole message with it, a single bad emoji on
/// one ticket type made the entire panel unpostable.
///
/// Custom emoji (`<:name:id>` / `<a:name:id>`) are passed through; Discord
/// still decides whether the bot may use that one. A unicode emoji has to
/// contain something outside ASCII and must not look like a shortcode or a
/// word, which is what separates `🎫` from `:ticket:`.
pub fn parse_button_emoji(raw: &str) -> Option<serenity::model::channel::ReactionType> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with('<') {
        return raw.parse().ok();
    }
    let looks_like_a_word = raw.chars().any(|c| c.is_ascii_alphabetic() || c == ':');
    let has_non_ascii = !raw.is_ascii();
    if looks_like_a_word || !has_non_ascii {
        return None;
    }
    raw.parse().ok()
}

#[cfg(test)]
mod emoji_tests {
    use super::parse_button_emoji;

    /// Real emoji, which is what people usually paste.
    #[test]
    fn plain_unicode_emoji_are_accepted() {
        // Every emoji seeded onto a ticket or application button is in here,
        // so a bad one is caught by the test run rather than by Discord
        // refusing the whole panel.
        for e in ["🎫", "🚨", "⚖️", "👮", "🛡️", "🪖", "⭐", "🐂", "⚙️", "🦅", "1️⃣", "🇬🇧", "👨‍👩‍👧"] {
            assert!(parse_button_emoji(e).is_some(), "{e} should be usable");
        }
    }

    /// The case that broke a whole ticket panel: serenity accepts these as
    /// unicode emoji, then Discord answers "Invalid Form Body ... Invalid
    /// emoji" and refuses the entire message.
    #[test]
    fn shortcodes_and_words_are_rejected() {
        for bad in [":ticket:", ":police:", "ticket", "report_player", "TICKET", "a", "::"] {
            assert!(parse_button_emoji(bad).is_none(), "{bad} must not reach Discord");
        }
    }

    #[test]
    fn custom_emoji_are_passed_through() {
        assert!(parse_button_emoji("<:guardian:1234567890>").is_some());
        assert!(parse_button_emoji("<a:spin:1234567890>").is_some());
    }

    /// Nothing configured is not an error, it just means a plain button.
    #[test]
    fn an_empty_emoji_is_simply_absent() {
        assert!(parse_button_emoji("").is_none());
        assert!(parse_button_emoji("   ").is_none());
    }

    /// Malformed custom emoji must not slip through as unicode.
    #[test]
    fn a_broken_custom_emoji_is_rejected() {
        for bad in ["<:noid:>", "<:missing", "<>", "<:name:notanumber>"] {
            assert!(parse_button_emoji(bad).is_none(), "{bad} must not reach Discord");
        }
    }
}
