//! Anti-Ping runtime state (persisted to SQLite `antiping`).
//!
//! A stored row holds the full effective config, so reading it back is a plain
//! deserialise; any field a stored row is missing (e.g. written by an older
//! version) falls back to the .env default via serde's container-level
//! `default`, which reproduces the JS `{ ...defaults, ...stored }` merge.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{root_file, CONFIG, GUILD_ID};
use crate::common::db;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default = "AntiPing::defaults")]
pub struct AntiPing {
    pub enabled: bool,
    /// none | warn | mute | timeout
    pub action: String,
    pub timeout_min: i64,
    pub delete_message: bool,
    pub ignore_replies: bool,
    pub notify_channel: bool,
    pub response_template: String,
    pub protected_users: Vec<String>,
    pub protected_roles: Vec<String>,
    /// Channels where pings aren't policed at all.
    pub exempt_channels: Vec<String>,
    /// Roles whose members may ping protected targets.
    pub exempt_roles: Vec<String>,
    /// After punishing someone, how long before the same person can be
    /// punished again. Stops one burst of pings from stacking timeouts and
    /// channel notices. 0 turns it off.
    pub cooldown_sec: i64,
}

impl AntiPing {
    /// The starting point for any server with no settings of its own.
    ///
    /// The protected users and roles from the .env are deliberately *not* in
    /// here. They name people and roles in the home server, and putting them in
    /// every server's defaults protected those users everywhere the bot is.
    /// They are seeded into the home server only, by `migrate_env_to_home_guild`.
    pub fn defaults() -> Self {
        Self {
            enabled: CONFIG.anti_ping_enabled,
            action: CONFIG.anti_ping_action.clone(),
            timeout_min: CONFIG.anti_ping_timeout_min,
            delete_message: CONFIG.anti_ping_delete_message,
            ignore_replies: CONFIG.anti_ping_ignore_replies,
            notify_channel: CONFIG.anti_ping_notify_channel,
            response_template: CONFIG.anti_ping_response.clone(),
            protected_users: Vec::new(),
            protected_roles: Vec::new(),
            exempt_channels: Vec::new(),
            exempt_roles: Vec::new(),
            cooldown_sec: 10,
        }
    }
}

static STORE: Lazy<Mutex<HashMap<String, AntiPing>>> = Lazy::new(|| {
    db::import_json_if_present("antiping", &root_file("antiping.json"));
    Mutex::new(db::load_all("antiping"))
});

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, AntiPing>> {
    match STORE.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// Effective per-guild anti-ping config: stored override → .env default.
pub fn ap(guild_id: &str) -> AntiPing {
    lock().get(guild_id).cloned().unwrap_or_else(AntiPing::defaults)
}

/// Returns whether the change reached the database.
pub fn update<F: FnOnce(&mut AntiPing)>(guild_id: &str, f: F) -> bool {
    let mut map = lock();
    let cfg = map.entry(guild_id.to_string()).or_insert_with(AntiPing::defaults);
    f(cfg);
    let snapshot = cfg.clone();
    drop(map);
    db::put("antiping", guild_id, &snapshot)
}

/// Seed the .env protected users/roles into the home guild (GUILD_ID) only,
/// and only when it has no anti-ping row yet.
pub fn migrate_env_to_home_guild() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    if CONFIG.anti_ping_protected_user_ids.is_empty() && CONFIG.anti_ping_protected_role_ids.is_empty() {
        return;
    }
    if lock().contains_key(home) {
        return;
    }
    update(home, |c| {
        c.protected_users = CONFIG.anti_ping_protected_user_ids.clone();
        c.protected_roles = CONFIG.anti_ping_protected_role_ids.clone();
    });
    println!("🔧 Seeded home guild ({home}) anti-ping protections from .env");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_guilds_keep_separate_anti_ping_settings() {
        update("test-ap-a", |c| {
            c.protected_users.push("1".into());
            c.action = "mute".into();
        });
        update("test-ap-b", |c| c.enabled = false);
        assert_eq!(ap("test-ap-a").protected_users, vec!["1".to_string()]);
        assert!(ap("test-ap-b").protected_users.is_empty());
        assert_eq!(ap("test-ap-a").action, "mute");
        assert!(ap("test-ap-a").enabled);
        assert!(!ap("test-ap-b").enabled);
    }

    #[test]
    fn an_unconfigured_guild_protects_nobody() {
        let d = ap("test-ap-never-configured");
        assert!(d.protected_users.is_empty());
        assert!(d.protected_roles.is_empty());
    }

    #[test]
    fn an_old_row_gains_the_new_fields_with_defaults() {
        let a: AntiPing = serde_json::from_str(r#"{"enabled":true,"protectedUsers":["5"]}"#).unwrap();
        assert_eq!(a.protected_users, vec!["5".to_string()]);
        assert!(a.exempt_channels.is_empty());
        assert_eq!(a.cooldown_sec, 10);
    }
}
