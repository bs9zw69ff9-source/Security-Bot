//! Environment configuration + shared constants.
//!
//! Behavioural thresholds are global (read once from the environment at
//! startup); per-guild identity settings live in `state::guild_settings`.

use once_cell::sync::Lazy;
use serenity::model::Permissions;
use std::collections::HashSet;
use std::path::PathBuf;

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}
fn env_int(key: &str, default: i64) -> i64 {
    env_str(key).and_then(|s| s.parse::<i64>().ok()).filter(|n| *n != 0).unwrap_or(default)
}
/// Mirrors the JS `process.env.X !== "false"` idiom: on unless explicitly "false".
fn env_bool_default_true(key: &str) -> bool {
    !matches!(env_str(key).as_deref(), Some("false"))
}
/// Mirrors the JS `process.env.X === "true"` idiom: off unless explicitly "true".
fn env_bool_default_false(key: &str) -> bool {
    matches!(env_str(key).as_deref(), Some("true"))
}
/// Like [`env_int`] but accepts an explicit `0` instead of treating it as unset -
/// needed wherever 0 is a meaningful value (e.g. "disable this check").
fn env_int_allow_zero(key: &str, default: i64) -> i64 {
    env_str(key).and_then(|s| s.parse::<i64>().ok()).unwrap_or(default)
}
fn env_csv(key: &str) -> Vec<String> {
    env_str(key)
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default()
}

fn root_dir() -> PathBuf {
    // Files live next to the crate (one level up from `rust/`), so a Rust
    // deployment reads/writes the same state files as the original bot.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}
pub fn root_file(name: &str) -> PathBuf {
    root_dir().join(name)
}

pub static TOKEN: Lazy<String> = Lazy::new(|| env_str("DISCORD_TOKEN").unwrap_or_default());
pub static GUILD_ID: Lazy<Option<String>> = Lazy::new(|| env_str("GUILD_ID"));

/// Always fully trusted: immune to anti-nuke, rate limits, and all guards.
/// Also unlocks the hidden, non-slash owner commands (!failsafe, !restore, etc).
/// BOT_OWNER_IDS (comma-separated) and/or BOT_OWNER_ID (singular, kept for
/// backward compatibility) are merged into one trusted set. Falls back to the
/// original hardcoded default if neither is set, so this still runs out of the
/// box - override it for any real deployment.
pub static BOT_OWNER_IDS: Lazy<HashSet<String>> = Lazy::new(|| {
    let mut ids: Vec<String> = env_csv("BOT_OWNER_IDS");
    if let Some(single) = env_str("BOT_OWNER_ID") {
        ids.push(single);
    }
    if ids.is_empty() {
        ids.push("1014251293159731310".to_string());
    }
    ids.into_iter().collect()
});

pub struct Config {
    pub log_channel_id: String,
    pub alert_channel_id: String,
    pub msg_log_channel_id: String,
    pub mod_role_id: String,
    pub mute_role_id: String,

    pub nuke_whitelist_role_ids: Vec<String>,
    pub nuke_whitelist_user_ids: Vec<String>,

    // Anti-spam
    pub spam_threshold: usize,
    pub spam_window_ms: i64,
    pub spam_mute_min: i64,
    pub spam_mention_limit: usize,
    pub spam_block_invites: bool,
    pub spam_duplicate_limit: usize,
    pub spam_exempt_staff: bool,

    // Anti-raid
    pub raid_join_threshold: usize,
    pub raid_window_ms: i64,
    pub raid_lockdown_min: i64,
    pub raid_kick_new_on_lock: bool,
    pub raid_min_account_age_min: i64,

    // Anti-nuke (fast window)
    pub nuke_window_ms: i64,
    pub nuke_channel_threshold: usize,
    pub nuke_channel_create_thresh: usize,
    pub nuke_role_threshold: usize,
    pub nuke_role_create_thresh: usize,
    pub nuke_ban_threshold: usize,
    pub nuke_kick_threshold: usize,
    pub nuke_webhook_threshold: usize,
    pub nuke_bot_add_action: String,
    pub nuke_emoji_threshold: usize,
    /// Shared across EVERY destructive action, so a nuke split across
    /// categories can't stay under each individual threshold. 0 disables it.
    pub nuke_total_threshold: usize,

    // Nuke recovery + hardening
    pub snapshot_interval_ms: u64,
    pub snapshot_max: usize,
    pub nuke_storm_threshold: usize,
    pub nuke_storm_window_ms: i64,
    pub scam_block: bool,
    pub owner_dm: bool,

    // Mod rate limits (rolling window; whitelisted users exempt)
    pub mod_ban_limit: usize,
    pub mod_kick_limit: usize,
    pub mod_mute_limit: usize,
    pub mod_purge_limit: usize,
    pub mod_lockdown_limit: usize,
    pub mod_warn_limit: usize,
    pub mod_window_ms: i64,

    // Warn escalation (0 = disabled)
    pub warn_mute_at: usize,
    pub warn_kick_at: usize,
    pub warn_ban_at: usize,
    pub warn_mute_min: i64,

    // Anti-ping
    pub anti_ping_enabled: bool,
    pub anti_ping_action: String,
    pub anti_ping_timeout_min: i64,
    pub anti_ping_delete_message: bool,
    pub anti_ping_ignore_replies: bool,
    pub anti_ping_notify_channel: bool,
    pub anti_ping_response: String,
    pub anti_ping_protected_user_ids: Vec<String>,
    pub anti_ping_protected_role_ids: Vec<String>,
}

pub static CONFIG: Lazy<Config> = Lazy::new(|| Config {
    log_channel_id: env_str("LOG_CHANNEL_ID").unwrap_or_default(),
    alert_channel_id: env_str("ALERT_CHANNEL_ID").unwrap_or_default(),
    msg_log_channel_id: env_str("MESSAGE_LOG_CHANNEL_ID").unwrap_or_default(),
    mod_role_id: env_str("MOD_ROLE_ID").unwrap_or_default(),
    mute_role_id: env_str("MUTE_ROLE_ID").unwrap_or_default(),

    nuke_whitelist_role_ids: env_csv("NUKE_WHITELIST_ROLE_IDS"),
    nuke_whitelist_user_ids: env_csv("NUKE_WHITELIST_USER_IDS"),

    spam_threshold: env_int("SPAM_THRESHOLD", 5) as usize,
    spam_window_ms: env_int("SPAM_WINDOW_MS", 3000),
    spam_mute_min: env_int("SPAM_MUTE_MIN", 10),
    spam_mention_limit: env_int("SPAM_MENTION_LIMIT", 6) as usize,
    spam_block_invites: env_bool_default_true("SPAM_BLOCK_INVITES"),
    spam_duplicate_limit: env_int("SPAM_DUPLICATE_LIMIT", 4) as usize,
    spam_exempt_staff: env_bool_default_true("SPAM_EXEMPT_STAFF"),

    raid_join_threshold: env_int("RAID_JOIN_THRESHOLD", 10) as usize,
    raid_window_ms: env_int("RAID_WINDOW_MS", 10000),
    raid_lockdown_min: env_int("RAID_LOCKDOWN_MIN", 5),
    raid_kick_new_on_lock: env_bool_default_true("RAID_KICK_NEW_ON_LOCK"),
    raid_min_account_age_min: env_int("RAID_MIN_ACCOUNT_AGE_MIN", 1440),

    nuke_window_ms: env_int("NUKE_WINDOW_MS", 10000),
    nuke_channel_threshold: env_int("NUKE_CHANNEL_THRESHOLD", 3) as usize,
    nuke_channel_create_thresh: env_int("NUKE_CHANNEL_CREATE_THRESH", 3) as usize,
    nuke_role_threshold: env_int("NUKE_ROLE_THRESHOLD", 3) as usize,
    nuke_role_create_thresh: env_int("NUKE_ROLE_CREATE_THRESH", 3) as usize,
    nuke_ban_threshold: env_int("NUKE_BAN_THRESHOLD", 3) as usize,
    nuke_kick_threshold: env_int("NUKE_KICK_THRESHOLD", 3) as usize,
    nuke_webhook_threshold: env_int("NUKE_WEBHOOK_THRESHOLD", 3) as usize,
    nuke_bot_add_action: env_str("NUKE_BOT_ADD_ACTION").unwrap_or_else(|| "kick".to_string()),
    nuke_emoji_threshold: env_int("NUKE_EMOJI_THRESHOLD", 3) as usize,
    nuke_total_threshold: env_int_allow_zero("NUKE_TOTAL_THRESHOLD", 3) as usize,

    snapshot_interval_ms: env_int("SNAPSHOT_INTERVAL_MS", 1_800_000) as u64,
    snapshot_max: env_int("SNAPSHOT_MAX", 5) as usize,
    nuke_storm_threshold: env_int("NUKE_STORM_THRESHOLD", 3) as usize,
    nuke_storm_window_ms: env_int("NUKE_STORM_WINDOW_MS", 60000),
    scam_block: env_bool_default_true("SCAM_BLOCK"),
    owner_dm: env_bool_default_true("OWNER_DM"),

    mod_ban_limit: env_int("MOD_BAN_LIMIT", 3) as usize,
    mod_kick_limit: env_int("MOD_KICK_LIMIT", 10) as usize,
    mod_mute_limit: env_int("MOD_MUTE_LIMIT", 20) as usize,
    mod_purge_limit: env_int("MOD_PURGE_LIMIT", 5) as usize,
    mod_lockdown_limit: env_int("MOD_LOCKDOWN_LIMIT", 5) as usize,
    mod_warn_limit: env_int("MOD_WARN_LIMIT", 30) as usize,
    mod_window_ms: env_int("MOD_WINDOW_MS", 86_400_000),

    warn_mute_at: env_int("WARN_MUTE_AT", 3) as usize,
    warn_kick_at: env_int("WARN_KICK_AT", 5) as usize,
    warn_ban_at: env_int("WARN_BAN_AT", 7) as usize,
    warn_mute_min: env_int("WARN_MUTE_MIN", 60),

    anti_ping_enabled: env_bool_default_true("ANTIPING_ENABLED"),
    anti_ping_action: env_str("ANTIPING_ACTION").unwrap_or_else(|| "timeout".to_string()),
    anti_ping_timeout_min: env_int("ANTIPING_TIMEOUT_MIN", 5),
    anti_ping_delete_message: env_bool_default_false("ANTIPING_DELETE"),
    anti_ping_ignore_replies: env_bool_default_true("ANTIPING_IGNORE_REPLIES"),
    anti_ping_notify_channel: env_bool_default_true("ANTIPING_NOTIFY"),
    anti_ping_response: env_str("ANTIPING_RESPONSE")
        .unwrap_or_else(|| "{user}, please don't ping {targets}. You have been {action}.".to_string()),
    anti_ping_protected_user_ids: env_csv("ANTIPING_PROTECTED_USER_IDS"),
    anti_ping_protected_role_ids: env_csv("ANTIPING_PROTECTED_ROLE_IDS"),
});

/// Dangerous permissions used across anti-nuke checks.
pub static DANGER_PERMS: Lazy<Vec<Permissions>> = Lazy::new(|| {
    vec![
        Permissions::ADMINISTRATOR,
        Permissions::MANAGE_GUILD,
        Permissions::MANAGE_CHANNELS,
        Permissions::MANAGE_ROLES,
        Permissions::MANAGE_WEBHOOKS,
        Permissions::BAN_MEMBERS,
        Permissions::KICK_MEMBERS,
    ]
});
/// Same set collapsed into one bitfield, for `.intersects()` checks.
pub static DANGER_PERMS_MASK: Lazy<Permissions> =
    Lazy::new(|| DANGER_PERMS.iter().fold(Permissions::empty(), |acc, p| acc | *p));

pub static INVITE_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"(?i)(discord\.(gg|io|me|li)|discordapp\.com/invite|discord\.com/invite)/[a-z0-9-]+").unwrap()
});

/// High-precision scam/grabber patterns (typo-squats + known IP grabbers).
/// Kept tight to avoid false positives.
pub static SCAM_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(
        r"(?i)(dlscord|disc0rd|discocrd|dlscordnitro|steamcommunilty|steancommunity|grabify\.link|iplogger\.(org|com|ru|co)|discordapp\.(ru|info)|free-?nitro-?gen|nitro-?free-?gift)",
    )
    .unwrap()
});

/// Milliseconds since the Unix epoch - the Rust equivalent of `Date.now()`,
/// which every stored timestamp in this bot uses.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
