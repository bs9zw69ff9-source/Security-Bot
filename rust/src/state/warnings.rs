//! Warnings state (persisted to SQLite `warnings`).
//!
//! Shape: `{ [guildId]: { [userId]: [{ reason, by, at }] } }`

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{now_ms, root_file};
use crate::common::db;

#[derive(Clone, Serialize, Deserialize)]
pub struct Warning {
    pub reason: String,
    /// User id of the moderator who issued it.
    pub by: String,
    pub at: i64,
}

type Store = HashMap<String, HashMap<String, Vec<Warning>>>;
static STORE: Lazy<Mutex<Store>> = Lazy::new(|| {
    db::import_json_if_present("warnings", &root_file("warnings.json"));
    Mutex::new(db::load_all("warnings"))
});

fn lock() -> std::sync::MutexGuard<'static, Store> {
    match STORE.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// Record a warning and return the member's new total.
pub fn add_warning(guild_id: &str, user_id: &str, reason: &str, by: &str) -> usize {
    let mut map = lock();
    let guild = map.entry(guild_id.to_string()).or_default();
    let list = guild.entry(user_id.to_string()).or_default();
    list.push(Warning { reason: reason.to_string(), by: by.to_string(), at: now_ms() });
    let total = list.len();
    let snapshot = guild.clone();
    drop(map);
    db::put("warnings", guild_id, &snapshot);
    total
}

pub fn get_warnings(guild_id: &str, user_id: &str) -> Vec<Warning> {
    lock().get(guild_id).and_then(|g| g.get(user_id)).cloned().unwrap_or_default()
}

pub fn clear_warnings(guild_id: &str, user_id: &str) {
    let mut map = lock();
    let Some(guild) = map.get_mut(guild_id) else { return };
    if guild.remove(user_id).is_none() {
        return;
    }
    let snapshot = guild.clone();
    drop(map);
    db::put("warnings", guild_id, &snapshot);
}
