//! Per-guild settings (set via /setup; override .env defaults).
//!
//! Field names serialise to the same camelCase keys the JS bot wrote, so an
//! existing `guardian.db` is read back unchanged.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::common::config::{root_file, CONFIG, GUILD_ID};
use crate::common::db;
use crate::state::tunables::{self, ModConfig, NukeConfig, Overrides, RaidConfig, SpamConfig};

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
    /// Same convention as `antiraid_disabled`.
    pub antinuke_disabled: bool,
    pub antispam_disabled: bool,
    /// This server's overrides of the .env thresholds (see `state::tunables`).
    pub thresholds: Overrides,
    /// ProBot-style log channels created by `/setup logs`: log type key
    /// (e.g. "memberBan") -> channel id. Same "logChannels" key the JS bot used.
    pub log_channels: HashMap<String, String>,
}

impl GuildSettings {
    pub fn nuke(&self) -> NukeConfig {
        tunables::nuke(&self.thresholds, !self.antinuke_disabled)
    }
    pub fn raid(&self) -> RaidConfig {
        tunables::raid(&self.thresholds, !self.antiraid_disabled)
    }
    pub fn spam(&self) -> SpamConfig {
        tunables::spam(&self.thresholds, !self.antispam_disabled)
    }
    pub fn moderation(&self) -> ModConfig {
        tunables::moderation(&self.thresholds)
    }
}

// Read on nearly every event, written only by configuration commands.
static SETTINGS: Lazy<RwLock<HashMap<String, GuildSettings>>> = Lazy::new(|| {
    db::import_json_if_present("guild_settings", &root_file("guildsettings.json"));
    RwLock::new(db::load_all("guild_settings"))
});

fn read() -> RwLockReadGuard<'static, HashMap<String, GuildSettings>> {
    SETTINGS.read().unwrap_or_else(|e| e.into_inner())
}

fn write() -> RwLockWriteGuard<'static, HashMap<String, GuildSettings>> {
    SETTINGS.write().unwrap_or_else(|e| e.into_inner())
}

/// Effective per-guild config - STRICTLY per server (no global fallback, so one
/// guild's channels/roles/whitelist can never leak into another).
pub fn gc(guild_id: &str) -> GuildSettings {
    read().get(guild_id).cloned().unwrap_or_default()
}

/// Read one guild's settings without cloning them.
pub fn with<R>(guild_id: &str, f: impl FnOnce(&GuildSettings) -> R) -> R {
    let map = read();
    match map.get(guild_id) {
        Some(s) => f(s),
        None => f(&GuildSettings::default()),
    }
}

pub fn nuke(guild_id: &str) -> NukeConfig {
    with(guild_id, GuildSettings::nuke)
}
pub fn raid(guild_id: &str) -> RaidConfig {
    with(guild_id, GuildSettings::raid)
}
pub fn spam(guild_id: &str) -> SpamConfig {
    with(guild_id, GuildSettings::spam)
}
pub fn moderation(guild_id: &str) -> ModConfig {
    with(guild_id, GuildSettings::moderation)
}

/// Apply a mutation to one guild's settings and persist it.
///
/// Returns whether the change reached the database, so a caller can avoid
/// reporting a save that did not happen.
pub fn update<F: FnOnce(&mut GuildSettings)>(guild_id: &str, f: F) -> bool {
    let mut map = write();
    let entry = map.entry(guild_id.to_string()).or_default();
    f(entry);
    let snapshot = entry.clone();
    drop(map);
    db::put("guild_settings", guild_id, &snapshot)
}

/// One-time backward-compat: if legacy .env identity values are set, seed them
/// into the HOME guild (GUILD_ID) ONLY - never applied globally, so other
/// servers stay clean.
pub fn migrate_env_to_home_guild() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    let mut map = write();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tunables::Tunable;

    #[test]
    fn two_guilds_keep_independent_settings() {
        let (a, b) = ("test-gs-a", "test-gs-b");
        update(a, |s| {
            s.mod_role_id = "111".into();
            s.log_channels.insert("memberBan".into(), "1001".into());
            s.thresholds.insert(Tunable::RaidJoinThreshold.key().into(), 4);
        });
        update(b, |s| {
            s.mod_role_id = "222".into();
            s.antinuke_disabled = true;
            s.log_channels.insert("memberBan".into(), "2002".into());
        });

        assert_eq!(gc(a).mod_role_id, "111");
        assert_eq!(gc(b).mod_role_id, "222");
        assert_eq!(gc(a).log_channels["memberBan"], "1001");
        assert_eq!(gc(b).log_channels["memberBan"], "2002");
        assert!(nuke(a).enabled);
        assert!(!nuke(b).enabled);
        assert_eq!(raid(a).join_threshold, 4);
        assert_eq!(raid(b).join_threshold, Tunable::RaidJoinThreshold.env_default() as usize);
    }

    #[test]
    fn an_unknown_guild_gets_defaults_not_another_guilds_settings() {
        update("test-gs-configured", |s| s.log_channel_id = "999".into());
        let fresh = gc("test-gs-never-seen");
        assert!(fresh.log_channel_id.is_empty());
        assert!(fresh.log_channels.is_empty());
        assert!(fresh.thresholds.is_empty());
    }

    /// What a restart does: everything is read back from the rows written.
    #[test]
    fn settings_survive_a_round_trip_through_the_database() {
        let gid = "test-gs-restart";
        update(gid, |s| {
            s.alert_channel_id = "42".into();
            s.antispam_disabled = true;
            s.thresholds.insert(Tunable::NukeBan.key().into(), 9);
        });
        let reloaded: HashMap<String, GuildSettings> = db::load_all("guild_settings");
        let s = &reloaded[gid];
        assert_eq!(s.alert_channel_id, "42");
        assert!(s.antispam_disabled);
        assert_eq!(s.nuke().ban, 9);
    }

    /// Rows written by older builds lack the new fields; they must still load.
    #[test]
    fn an_old_row_without_the_new_fields_still_parses() {
        let s: GuildSettings = serde_json::from_str(r#"{"modRoleId":"5","antiraidDisabled":true}"#).unwrap();
        assert_eq!(s.mod_role_id, "5");
        assert!(!s.raid().enabled);
        assert!(s.nuke().enabled);
        assert!(s.thresholds.is_empty());
    }
}
