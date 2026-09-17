//! Muted-role stash state (persisted to SQLite `muted_roles`).
//!
//! Shape: `{ [guildId]: { [userId]: { roles, reason, mutedAt, expiresAt } } }`

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::root_file;
use crate::common::db;

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MuteStash {
    /// Role ids stripped on mute, handed back on unmute.
    pub roles: Vec<String>,
    pub reason: String,
    pub muted_at: i64,
    /// `None` for a permanent mute (cleared only by an explicit /unmute).
    pub expires_at: Option<i64>,
}

type Store = HashMap<String, HashMap<String, MuteStash>>;
static STORE: Lazy<Mutex<Store>> = Lazy::new(|| {
    db::import_json_if_present("muted_roles", &root_file("mutedroles.json"));
    Mutex::new(db::load_all("muted_roles"))
});

fn lock() -> std::sync::MutexGuard<'static, Store> {
    match STORE.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

pub fn get(guild_id: &str, user_id: &str) -> Option<MuteStash> {
    lock().get(guild_id).and_then(|g| g.get(user_id)).cloned()
}

/// Number of roles currently stashed for a user (0 if none) - used for the
/// "I've set aside N roles" wording on /mute and /unmute.
pub fn stashed_count(guild_id: &str, user_id: &str) -> usize {
    get(guild_id, user_id).map(|s| s.roles.len()).unwrap_or(0)
}

pub fn set(guild_id: &str, user_id: &str, stash: MuteStash) {
    let mut map = lock();
    let guild = map.entry(guild_id.to_string()).or_default();
    guild.insert(user_id.to_string(), stash);
    let snapshot = guild.clone();
    drop(map);
    db::put("muted_roles", guild_id, &snapshot);
}

pub fn remove(guild_id: &str, user_id: &str) {
    let mut map = lock();
    let Some(guild) = map.get_mut(guild_id) else { return };
    guild.remove(user_id);
    if guild.is_empty() {
        map.remove(guild_id);
        drop(map);
        db::delete("muted_roles", guild_id);
        return;
    }
    let snapshot = guild.clone();
    drop(map);
    db::put("muted_roles", guild_id, &snapshot);
}

/// Every stored mute, for boot recovery.
pub fn all() -> Vec<(String, Vec<(String, MuteStash)>)> {
    lock()
        .iter()
        .map(|(gid, users)| (gid.clone(), users.iter().map(|(uid, s)| (uid.clone(), s.clone())).collect()))
        .collect()
}
