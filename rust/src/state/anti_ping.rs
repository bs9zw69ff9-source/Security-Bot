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

use crate::common::config::{root_file, CONFIG};
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
}

impl AntiPing {
    pub fn defaults() -> Self {
        Self {
            enabled: CONFIG.anti_ping_enabled,
            action: CONFIG.anti_ping_action.clone(),
            timeout_min: CONFIG.anti_ping_timeout_min,
            delete_message: CONFIG.anti_ping_delete_message,
            ignore_replies: CONFIG.anti_ping_ignore_replies,
            notify_channel: CONFIG.anti_ping_notify_channel,
            response_template: CONFIG.anti_ping_response.clone(),
            protected_users: CONFIG.anti_ping_protected_user_ids.clone(),
            protected_roles: CONFIG.anti_ping_protected_role_ids.clone(),
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

pub fn update<F: FnOnce(&mut AntiPing)>(guild_id: &str, f: F) {
    let mut cfg = ap(guild_id);
    f(&mut cfg);
    lock().insert(guild_id.to_string(), cfg.clone());
    db::put("antiping", guild_id, &cfg);
}
