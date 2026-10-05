//! Guardian's visual system.
//!
//! One bold palette, one icon set and one layout for every embed the bot
//! sends, so a moderation card, a security log and a "you can't do that"
//! reply all read as the same product:
//!
//!   colour bar  → what kind of thing this is (success, danger, security, …)
//!   title       → `<icon>  <headline>`
//!   body        → the message, with details in labelled fields
//!   footer      → `🛡️ Guardian • <section>` plus a timestamp
//!
//! Everything here is pure (no Discord I/O) so the layout can be unit-tested.

use serenity::builder::{CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter};
use serenity::model::Timestamp;

/// The palette: all blue, one shade per meaning, so the bot reads as one
/// colour while each kind of message still stands apart in a busy channel.
/// Darker means more serious.
pub mod palette {
    pub const SKY: u32 = 0x38BDF8; // success, restored, unbanned
    pub const ROYAL: u32 = 0x1D4ED8; // bans, kicks, errors
    pub const ICE: u32 = 0x93C5FD; // warnings, purges, heads-ups
    pub const AZURE: u32 = 0x3B82F6; // information, status
    pub const INDIGO: u32 = 0x4F46E5; // moderation in general
    pub const COBALT: u32 = 0x2563EB; // security systems (anti-spam, raid, ping)
    pub const NAVY: u32 = 0x1E3A8A; // anti-nuke, critical alerts
    pub const CERULEAN: u32 = 0x0EA5E9; // mutes
    pub const CYAN: u32 = 0x06B6D4; // tickets
    pub const POWDER: u32 = 0x7DD3FC; // applications
    pub const SAPPHIRE: u32 = 0x3B5BDB; // setup / configuration
    pub const MIDNIGHT: u32 = 0x172554; // neutral
}

pub const BRAND: &str = "Guardian";
const BRAND_ICON: &str = "🛡️";

/// The kind of reply, which decides its colour, icon and default headline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Success,
    Error,
    Warning,
    Info,
    Denied,
}

impl Tone {
    pub fn color(self) -> u32 {
        match self {
            Tone::Success => palette::SKY,
            Tone::Error => palette::ROYAL,
            Tone::Warning => palette::ICE,
            Tone::Info => palette::AZURE,
            Tone::Denied => palette::NAVY,
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            Tone::Success => "✅",
            Tone::Error => "❌",
            Tone::Warning => "⚠️",
            Tone::Info => "💡",
            Tone::Denied => "🔒",
        }
    }
    pub fn headline(self) -> &'static str {
        match self {
            Tone::Success => "Done",
            Tone::Error => "That didn't work",
            Tone::Warning => "Heads up",
            Tone::Info => "Good to know",
            Tone::Denied => "Access denied",
        }
    }

    /// Work out the tone of a plain-text reply from its wording, so the ~70
    /// short text replies across the bot pick up the right colour without
    /// each call site having to say.
    pub fn infer(text: &str) -> Tone {
        let t = text.trim_start();
        let lower = t.to_lowercase();
        if lower.starts_with("only the") || lower.contains("staff only") || lower.contains("owner only") {
            return Tone::Denied;
        }
        if lower.starts_with("done") || lower.starts_with("refreshed") || lower.contains(" is up in <#") {
            return Tone::Success;
        }
        if t.starts_with('⚠') || lower.starts_with("hold on") || lower.contains("already") {
            return Tone::Warning;
        }
        if lower.contains("clean slate")
            || lower.starts_with("no ")
            || lower.starts_with("panic lockdown is on")
            || lower.contains(" yet.")
        {
            return Tone::Info;
        }
        Tone::Error
    }
}

/// `🛡️ Guardian • <section>`.
pub fn footer(section: &str) -> CreateEmbedFooter {
    if section.is_empty() {
        CreateEmbedFooter::new(format!("{BRAND_ICON} {BRAND}"))
    } else {
        CreateEmbedFooter::new(format!("{BRAND_ICON} {BRAND} • {section}"))
    }
}

/// `<icon>  <title>`, unless the title already starts with its own icon.
pub fn titled(icon: &str, title: &str) -> String {
    let starts_with_symbol = title.chars().next().map(|c| !c.is_alphanumeric()).unwrap_or(false);
    if starts_with_symbol || icon.is_empty() {
        title.to_string()
    } else {
        format!("{icon}  {title}")
    }
}

/// A standard Guardian card.
pub fn card(tone: Tone, title: Option<&str>, body: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new()
        .color(tone.color())
        .title(titled(tone.icon(), title.unwrap_or(tone.headline())))
        .description(body)
        .footer(footer(""))
        .timestamp(Timestamp::now())
}

/// The icon that goes with one of the palette colours, for embeds that only
/// know their colour.
pub fn icon_for_color(color: u32) -> &'static str {
    match color {
        palette::SKY => "✅",
        palette::ROYAL => "⛔",
        palette::ICE => "⚠️",
        palette::AZURE => "💡",
        palette::NAVY => "☢️",
        palette::CERULEAN => "🔇",
        palette::COBALT => "🚨",
        palette::CYAN => "🎫",
        palette::POWDER => "📝",
        palette::SAPPHIRE => "⚙️",
        _ => BRAND_ICON,
    }
}

/// Give an embed the Guardian footer and a timestamp when it has neither.
/// Embeds that set their own footer (usage bars, hints) keep it.
pub fn finish(e: CreateEmbed) -> CreateEmbed {
    let json = serde_json::to_value(&e).unwrap_or_default();
    let mut e = e;
    if json.get("footer").map(|f| f.is_null()).unwrap_or(true) {
        e = e.footer(footer(""));
    }
    if json.get("timestamp").map(|t| t.is_null()).unwrap_or(true) {
        e = e.timestamp(Timestamp::now());
    }
    e
}

// ── Moderation cards ──────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModAction {
    Mute,
    Unmute,
    Kick,
    Ban,
    Unban,
    Warn,
    Purge,
}

impl ModAction {
    pub fn icon(self) -> &'static str {
        match self {
            ModAction::Mute => "🔇",
            ModAction::Unmute => "🔊",
            ModAction::Kick => "👢",
            ModAction::Ban => "🔨",
            ModAction::Unban => "♻️",
            ModAction::Warn => "⚠️",
            ModAction::Purge => "🗑️",
        }
    }
    pub fn color(self) -> u32 {
        match self {
            ModAction::Mute => palette::CERULEAN,
            ModAction::Unmute | ModAction::Unban => palette::SKY,
            ModAction::Kick | ModAction::Ban => palette::ROYAL,
            ModAction::Warn | ModAction::Purge => palette::ICE,
        }
    }
    pub fn headline(self) -> &'static str {
        match self {
            ModAction::Mute => "Member Muted",
            ModAction::Unmute => "Member Unmuted",
            ModAction::Kick => "Member Kicked",
            ModAction::Ban => "Member Banned",
            ModAction::Unban => "Member Unbanned",
            ModAction::Warn => "Warning Issued",
            ModAction::Purge => "Messages Cleared",
        }
    }
    fn past(self) -> &'static str {
        match self {
            ModAction::Mute => "muted",
            ModAction::Unmute => "unmuted",
            ModAction::Kick => "kicked",
            ModAction::Ban => "banned",
            ModAction::Unban => "unbanned",
            ModAction::Warn => "warned",
            ModAction::Purge => "cleared",
        }
    }
}

/// Who a moderation card is about.
pub struct Subject<'a> {
    pub id: u64,
    pub tag: Option<&'a str>,
    pub avatar: Option<String>,
}

/// The moderation case card: a big coloured headline, the member's avatar,
/// then Member / Moderator / Reason laid out as fields.
pub fn mod_card(
    action: ModAction,
    target: &Subject,
    moderator: u64,
    reason: Option<&str>,
    extra: &[(&str, String)],
) -> CreateEmbed {
    let mut author = CreateEmbedAuthor::new(format!("{} {}", action.icon(), action.headline().to_uppercase()));
    if let Some(a) = &target.avatar {
        author = author.icon_url(a);
    }
    let who = match target.tag {
        Some(tag) => format!("<@{}>\n`{tag}`", target.id),
        None => format!("<@{}>\n`{}`", target.id, target.id),
    };
    let mut e = CreateEmbed::new()
        .color(action.color())
        .author(author)
        .description(format!("**<@{}> has been {}.**", target.id, action.past()))
        .field("👤 Member", who, true)
        .field("🛡️ Moderator", format!("<@{moderator}>"), true);
    if let Some(r) = reason {
        e = e.field("📝 Reason", format!(">>> {r}"), false);
    }
    for (name, value) in extra {
        e = e.field(*name, value.clone(), false);
    }
    if let Some(a) = &target.avatar {
        e = e.thumbnail(a);
    }
    e.footer(footer("Moderation")).timestamp(Timestamp::now())
}

/// What a punished member receives in DMs.
pub fn dm_notice(action: ModAction, server: &str, reason: &str, extra: Option<&str>) -> CreateEmbed {
    let mut e = CreateEmbed::new()
        .color(action.color())
        .title(format!("{}  You were {} in {server}", action.icon(), action.past()))
        .field("🏠 Server", server, true)
        .field("📝 Reason", reason, true);
    if let Some(x) = extra {
        e = e.description(x);
    }
    e.footer(footer("This is an automated message")).timestamp(Timestamp::now())
}

// ── Security log ──────────────────────────────────────────────

/// Icon and section for a security-log title, so every log line carries a
/// consistent badge without each caller having to pick one.
pub fn log_badge(title: &str) -> (&'static str, &'static str) {
    let t = title.to_lowercase();
    match () {
        _ if t.contains("anti-nuke") => ("☢️", "Anti-Nuke"),
        _ if t.contains("anti-spam") => ("🧹", "Anti-Spam"),
        _ if t.contains("anti-ping") => ("📵", "Anti-Ping"),
        _ if t.contains("raid") => ("🚨", "Anti-Raid"),
        _ if t.contains("lockdown") || t.contains("locked") => ("🔒", "Lockdown"),
        _ if t.contains("unban") => ("♻️", "Moderation"),
        _ if t.contains("ban") => ("🔨", "Moderation"),
        _ if t.contains("kick") => ("👢", "Moderation"),
        _ if t.contains("unmute") || t.contains("restored") => ("🔊", "Moderation"),
        _ if t.contains("mute") => ("🔇", "Moderation"),
        _ if t.contains("warnings cleared") => ("🧽", "Moderation"),
        _ if t.contains("warn") => ("⚠️", "Moderation"),
        _ if t.contains("escalation") => ("📈", "Moderation"),
        _ if t.contains("purge") => ("🗑️", "Moderation"),
        _ if t.contains("ticket") => ("🎫", "Tickets"),
        _ if t.contains("application") => ("📝", "Applications"),
        _ if t.contains("snapshot") || t.contains("rollback") || t.contains("restore") => ("📸", "Recovery"),
        _ => (BRAND_ICON, "Security"),
    }
}

/// A security-log entry.
pub fn log_card(title: &str, desc: &str, color: u32) -> CreateEmbed {
    let (icon, section) = log_badge(title);
    CreateEmbed::new()
        .color(color)
        .author(CreateEmbedAuthor::new(format!("{BRAND_ICON} SECURITY LOG • {}", section.to_uppercase())))
        .title(titled(icon, title))
        .description(desc)
        .footer(footer(section))
        .timestamp(Timestamp::now())
}

/// A critical alert: louder than a log line, and it names the server so a DM
/// copy still makes sense out of context.
pub fn alert_card(title: &str, desc: &str, color: u32, server: Option<&str>) -> CreateEmbed {
    let mut e = CreateEmbed::new()
        .color(color)
        .author(CreateEmbedAuthor::new("🚨 CRITICAL ALERT"))
        .title(titled("🚨", title))
        .description(desc);
    if let Some(s) = server {
        e = e.field("🏠 Server", s, true);
    }
    e.footer(footer("Critical Alert")).timestamp(Timestamp::now())
}

// ── Help ──────────────────────────────────────────────────────

/// One section of `/help`: its own colour, so the menu reads as a set of
/// coloured panels rather than one wall of text.
pub struct HelpSection {
    pub icon: &'static str,
    pub name: &'static str,
    pub color: u32,
    pub commands: &'static [(&'static str, &'static str)],
}

pub const HELP_SECTIONS: &[HelpSection] = &[
    HelpSection {
        icon: "🔨",
        name: "Moderation",
        color: palette::INDIGO,
        commands: &[
            ("/mute `@user [minutes] [reason]`", "Mute - roles are stashed and handed back on unmute"),
            ("/unmute `@user`", "Unmute and restore stashed roles"),
            ("/kick `@user [reason]`", "Kick a member"),
            ("/ban `@user [reason] [delete_days]`", "Ban a member"),
            ("/unban `user_id [reason]`", "Lift a ban by user ID"),
            ("/warn `@user [reason]`", "Warn - auto-escalates to mute, kick, ban"),
            ("/warnings `@user`", "See a member's warnings"),
            ("/clearwarns `@user`", "Wipe a member's warnings"),
            ("/purge `count [user]`", "Bulk-delete messages"),
            ("/lockdown `lock|unlock [channel]`", "Lock or unlock a channel"),
        ],
    },
    HelpSection {
        icon: "🚨",
        name: "Protection",
        color: palette::NAVY,
        commands: &[
            ("/panic", "Lock **every** text channel at once *(admin)*"),
            ("/antiraid `status|enable|disable`", "Raid protection for this server *(owner)*"),
            ("/antiping", "Stop pings to protected staff & VIPs *(admin)*"),
            ("/nuketest", "Check anti-nuke and my permissions *(admin)*"),
        ],
    },
    HelpSection {
        icon: "⚙️",
        name: "Setup",
        color: palette::SAPPHIRE,
        commands: &[
            ("/setup quick", "Mute role + log channels in one step"),
            ("/setup logs", "A 🗃️│ channel for every log type"),
            ("/setup view", "Everything that's configured here"),
            ("/setup roles · channels", "Set individual pieces"),
            ("/setup whitelist · failsafe", "Anti-nuke whitelist & `!failsafe` roles *(owner)*"),
            ("/config", "This server's thresholds & module switches *(owner)*"),
        ],
    },
    HelpSection {
        icon: "🏛️",
        name: "Community",
        color: palette::CYAN,
        commands: &[
            ("/police manual setup", "Post the officer guide & procedures manual"),
            ("/chainofcommand setup · setroles · setgroup", "Auto-updating chain-of-command board *(admin)*"),
        ],
    },
    HelpSection {
        icon: "📊",
        name: "Info",
        color: palette::AZURE,
        commands: &[
            ("/userinfo `id`", "Look up any user by ID or mention"),
            ("/limits", "Your mod actions left in the current window"),
            ("/status", "Uptime, ping, memory, guilds *(admin)*"),
            ("/servers", "DM me an invite to every server *(bot owner)*"),
            ("/help", "This menu"),
        ],
    },
];

/// `/help`: a header card followed by one coloured card per section.
pub fn help_cards(window_hours: i64, avatar: Option<String>) -> Vec<CreateEmbed> {
    let total: usize = HELP_SECTIONS.iter().map(|s| s.commands.len()).sum();
    let mut header = CreateEmbed::new()
        .color(palette::COBALT)
        .title(format!("{BRAND_ICON}  GUARDIAN • COMMAND CENTER"))
        .description(format!(
            "**Anti-nuke · anti-raid · anti-spam · moderation · server logs**\n\
             {total} commands across {} sections below.\n\n\
             🚀 **New here?** Run `/setup quick`, then `/setup logs`.\n\
             ⏱️ Mod actions are rate-limited over a rolling **{window_hours}h** - `/limits` shows yours.",
            HELP_SECTIONS.len()
        ));
    if let Some(a) = avatar {
        header = header.thumbnail(a);
    }
    let mut cards = vec![header];
    for s in HELP_SECTIONS {
        let body = s.commands.iter().map(|(cmd, what)| format!("**{cmd}**\n└ {what}")).collect::<Vec<_>>().join("\n");
        cards.push(CreateEmbed::new().color(s.color).title(format!("{}  {}", s.icon, s.name.to_uppercase())).description(body));
    }
    if let Some(last) = cards.pop() {
        cards.push(last.footer(footer("Help")).timestamp(Timestamp::now()));
    }
    cards
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(e: CreateEmbed) -> serde_json::Value {
        serde_json::to_value(e).unwrap()
    }

    #[test]
    fn tone_is_inferred_from_wording() {
        assert_eq!(Tone::infer("Only the bot owner or the server owner can change these settings."), Tone::Denied);
        assert_eq!(Tone::infer("This one is staff only - you need the mod role."), Tone::Denied);
        assert_eq!(Tone::infer("This one's owner only."), Tone::Denied);
        assert_eq!(Tone::infer("Done - the ticket panel is up in <#1> with: a."), Tone::Success);
        assert_eq!(Tone::infer("Refreshed."), Tone::Success);
        assert_eq!(Tone::infer("Hold on - that just tripped the anti-nuke protection."), Tone::Warning);
        assert_eq!(Tone::infer("⚠️ <@1> is already protected."), Tone::Warning);
        assert_eq!(Tone::infer("Anti-raid is already on here."), Tone::Warning);
        assert_eq!(Tone::infer("<@1> has a clean slate - no warnings."), Tone::Info);
        assert_eq!(Tone::infer("No ticket types yet. Add one with `/tickets addtype`."), Tone::Info);
        assert_eq!(Tone::infer("I can't find that user in this server."), Tone::Error);
        assert_eq!(Tone::infer("Give me at least one role to set."), Tone::Error);
    }

    #[test]
    fn cards_carry_brand_footer_and_icon() {
        let j = json(card(Tone::Error, None, "nope"));
        assert_eq!(j["title"], "❌  That didn't work");
        assert_eq!(j["color"], palette::ROYAL);
        assert_eq!(j["footer"]["text"], "🛡️ Guardian");
        assert!(j["timestamp"].is_string());
    }

    #[test]
    fn titled_does_not_double_up_icons() {
        assert_eq!(titled("✅", "Saved"), "✅  Saved");
        assert_eq!(titled("✅", "🔇 Member Muted"), "🔇 Member Muted");
    }

    #[test]
    fn finish_keeps_an_existing_footer() {
        let custom = json(finish(CreateEmbed::new().footer(CreateEmbedFooter::new("usage"))));
        assert_eq!(custom["footer"]["text"], "usage");
        let bare = json(finish(CreateEmbed::new().description("x")));
        assert_eq!(bare["footer"]["text"], "🛡️ Guardian");
        assert!(bare["timestamp"].is_string());
    }

    #[test]
    fn mod_card_layout() {
        let t = Subject { id: 5, tag: Some("alice"), avatar: Some("https://cdn/a.png".into()) };
        let j = json(mod_card(ModAction::Ban, &t, 9, Some("spam"), &[]));
        assert_eq!(j["author"]["name"], "🔨 MEMBER BANNED");
        assert_eq!(j["description"], "**<@5> has been banned.**");
        assert_eq!(j["fields"][0]["name"], "👤 Member");
        assert_eq!(j["fields"][1]["value"], "<@9>");
        assert_eq!(j["fields"][2]["value"], ">>> spam");
        assert_eq!(j["thumbnail"]["url"], "https://cdn/a.png");
        assert_eq!(j["color"], palette::ROYAL);
    }

    #[test]
    fn log_badges() {
        assert_eq!(log_badge("Member Banned"), ("🔨", "Moderation"));
        assert_eq!(log_badge("Member Unbanned"), ("♻️", "Moderation"));
        assert_eq!(log_badge("Member Unmuted"), ("🔊", "Moderation"));
        assert_eq!(log_badge("Anti-Nuke"), ("☢️", "Anti-Nuke"));
        assert_eq!(log_badge("Raid Quarantine"), ("🚨", "Anti-Raid"));
        assert_eq!(log_badge("Ticket Opened"), ("🎫", "Tickets"));
        assert_eq!(log_badge("Something Else"), ("🛡️", "Security"));
    }

    #[test]
    fn help_is_one_card_per_section_plus_header() {
        let cards = help_cards(24, None);
        assert_eq!(cards.len(), HELP_SECTIONS.len() + 1);
        assert!(cards.len() <= 10, "Discord allows at most 10 embeds per message");
        let last = json(cards.last().unwrap().clone());
        assert_eq!(last["footer"]["text"], "🛡️ Guardian • Help");
        let mut total = 0;
        for c in &cards {
            let d = json(c.clone());
            let desc = d["description"].as_str().unwrap().chars().count();
            assert!(desc <= 4096);
            total += desc + d["title"].as_str().unwrap_or("").chars().count();
            total += d["footer"]["text"].as_str().unwrap_or("").chars().count();
        }
        assert!(total <= 6000, "a message's embeds may hold 6000 characters in total, help uses {total}");
    }
}
