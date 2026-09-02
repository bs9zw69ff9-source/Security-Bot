//! Per-guild settings (set via /setup; override .env defaults).
//!
//! Field names serialise to the same camelCase keys the JS bot wrote, so an
//! existing `guardian.db` is read back unchanged.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{root_file, CONFIG, GUILD_ID};
use crate::common::db;

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GuildSettings {
    pub mod_role_id: String,
    pub mute_role_id: String,
    pub log_channel_id: String,
    pub alert_channel_id: String,
    pub msg_log_channel_id: String,
    pub nuke_whitelist_role_ids: Vec<String>,
    pub nuke_whitelist_user_ids: Vec<String>,
    pub failsafe_role_ids: Vec<String>,
    /// Anti-raid off for this server. Stored as "disabled" rather than
    /// "enabled" so it defaults to false, which means every existing row keeps
    /// the protection it already had.
    pub antiraid_disabled: bool,
}

static SETTINGS: Lazy<Mutex<HashMap<String, GuildSettings>>> = Lazy::new(|| {
    db::import_json_if_present("guild_settings", &root_file("guildsettings.json"));
    Mutex::new(db::load_all("guild_settings"))
});

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, GuildSettings>> {
    match SETTINGS.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// Effective per-guild config - STRICTLY per server (no global fallback, so one
/// guild's channels/roles/whitelist can never leak into another).
pub fn gc(guild_id: &str) -> GuildSettings {
    lock().get(guild_id).cloned().unwrap_or_default()
}

/// Apply a mutation to one guild's settings and persist it.
pub fn update<F: FnOnce(&mut GuildSettings)>(guild_id: &str, f: F) {
    let mut map = lock();
    let entry = map.entry(guild_id.to_string()).or_default();
    f(entry);
    let snapshot = entry.clone();
    drop(map);
    db::put("guild_settings", guild_id, &snapshot);
}

/// One-time backward-compat: if legacy .env identity values are set, seed them
/// into the HOME guild (GUILD_ID) ONLY - never applied globally, so other
/// servers stay clean.
pub fn migrate_env_to_home_guild() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    let mut map = lock();
    let cur = map.entry(home.clone()).or_default();
    let mut changed = false;

    // Only fill a field that has never been set (empty), matching the JS
    // `cur[k] === undefined` guard - a deliberate later blank is not clobbered
    // any more than it was before, since both start from the same default.
    let fill = |slot: &mut String, val: &str, changed: &mut bool| {
        if !val.is_empty() && slot.is_empty() {
            *slot = val.to_string();
            *changed = true;
        }
    };
    fill(&mut cur.mod_role_id, &CONFIG.mod_role_id, &mut changed);
    fill(&mut cur.mute_role_id, &CONFIG.mute_role_id, &mut changed);
    fill(&mut cur.log_channel_id, &CONFIG.log_channel_id, &mut changed);
    fill(&mut cur.alert_channel_id, &CONFIG.alert_channel_id, &mut changed);
    fill(&mut cur.msg_log_channel_id, &CONFIG.msg_log_channel_id, &mut changed);
    if !CONFIG.nuke_whitelist_role_ids.is_empty() && cur.nuke_whitelist_role_ids.is_empty() {
        cur.nuke_whitelist_role_ids = CONFIG.nuke_whitelist_role_ids.clone();
        changed = true;
    }
    if !CONFIG.nuke_whitelist_user_ids.is_empty() && cur.nuke_whitelist_user_ids.is_empty() {
        cur.nuke_whitelist_user_ids = CONFIG.nuke_whitelist_user_ids.clone();
        changed = true;
    }

    if changed {
        let snapshot = cur.clone();
        drop(map);
        db::put("guild_settings", home, &snapshot);
        println!("🔧 Seeded home guild ({home}) settings from .env");
    }
}
