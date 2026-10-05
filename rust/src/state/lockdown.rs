//! Lockdown state (persisted to SQLite `lockdown_state`).

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::now_ms;
use crate::common::db;

/// One permission overwrite the lockdown edited, and exactly what it changed.
///
/// Restoring from this instead of blanket-clearing the permission is what keeps
/// a lift from opening channels that were deliberately read-only long before
/// the lockdown started.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedTarget {
    /// Role or member id the overwrite belongs to.
    pub id: String,
    /// "role" | "member"
    pub kind: String,
    /// Bits this lockdown added to `deny`, as a decimal string.
    pub denied: String,
    /// Bits this lockdown removed from `allow`, as a decimal string.
    pub allow_removed: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedChannel {
    pub channel_id: String,
    pub targets: Vec<LockedTarget>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockdownState {
    /// "raid" | "panic" | "manual". "nukestorm" also appears in databases
    /// written before the storm escalation was removed; it is still honoured on
    /// boot so an old lock does not silently lift itself.
    pub reason: String,
    pub locked_at: i64,
    /// `None` for manual/panic locks, which never auto-expire.
    pub expires_at: Option<i64>,
    /// What this lockdown actually changed. Empty on locks written before this
    /// was recorded, which the lift path treats as "unknown" rather than
    /// "nothing".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<LockedChannel>,
    /// Started by a bot owner or the server owner, so only one of them may
    /// lift it early. A server admin can lift a panic another admin started,
    /// but not one an owner started to contain them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owner_locked: bool,
}

static STATE: Lazy<Mutex<HashMap<String, LockdownState>>> = Lazy::new(|| Mutex::new(db::load_all("lockdown_state")));

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, LockdownState>> {
    match STATE.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

pub fn is_lockdown(guild_id: &str) -> bool {
    lock().contains_key(guild_id)
}

/// Why this guild is locked down, if it is. Lets a caller act on a raid lock
/// without disturbing a manual or panic one.
pub fn lockdown_reason(guild_id: &str) -> Option<String> {
    lock().get(guild_id).map(|s| s.reason.clone())
}

/// Whether the active lockdown was started by an owner.
pub fn is_owner_locked(guild_id: &str) -> bool {
    lock().get(guild_id).is_some_and(|s| s.owner_locked)
}

pub fn locked_count() -> usize {
    lock().len()
}

/// The active lockdown, if any.
pub fn get(guild_id: &str) -> Option<LockdownState> {
    lock().get(guild_id).cloned()
}

/// Mark the guild locked unless it already is, and return the new lock's
/// `locked_at`. The check and the set happen under one lock, so two raid
/// triggers arriving together can't both start a lockdown pass.
pub fn try_set_lockdown(guild_id: &str, reason: &str, expires_at: Option<i64>) -> Option<i64> {
    try_set_lockdown_by(guild_id, reason, expires_at, false)
}

/// [`try_set_lockdown`], recording whether an owner started it.
pub fn try_set_lockdown_by(guild_id: &str, reason: &str, expires_at: Option<i64>, owner_locked: bool) -> Option<i64> {
    let mut map = lock();
    if map.contains_key(guild_id) {
        return None;
    }
    let state = LockdownState { reason: reason.to_string(), locked_at: now_ms(), expires_at, changed: Vec::new(), owner_locked };
    let locked_at = state.locked_at;
    map.insert(guild_id.to_string(), state.clone());
    drop(map);
    db::put("lockdown_state", guild_id, &state);
    Some(locked_at)
}

/// Whether the lockdown a timer was started for is still the one in force.
///
/// Auto-lift timers sleep for minutes. In that time the lock can be lifted by
/// hand and a new one started, and the old timer must not lift the new one.
pub fn is_same_lock(guild_id: &str, locked_at: i64) -> bool {
    lock().get(guild_id).map(|s| s.locked_at == locked_at).unwrap_or(false)
}

/// What the active lockdown changed, if anything is on record.
pub fn changed_channels(guild_id: &str) -> Option<Vec<LockedChannel>> {
    lock().get(guild_id).map(|s| s.changed.clone())
}

/// Attach the change record to a lockdown that is already marked active,
/// keeping its original `locked_at` and expiry.
///
/// Locking runs one HTTP call per overwrite, so the lockdown is marked first to
/// keep a second trigger from starting its own pass, and the record lands when
/// that work finishes.
pub fn record_changes(guild_id: &str, changed: Vec<LockedChannel>) {
    let mut map = lock();
    if let Some(state) = map.get_mut(guild_id) {
        state.changed = changed;
        let snapshot = state.clone();
        drop(map);
        db::put("lockdown_state", guild_id, &snapshot);
    }
}

pub fn clear_lockdown(guild_id: &str) {
    lock().remove(guild_id);
    db::delete("lockdown_state", guild_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An owner's panic is marked as theirs, so an admin can't lift it, while
    /// one an admin started (or anti-raid's) isn't.
    #[test]
    fn an_owner_started_lockdown_is_marked_as_theirs() {
        try_set_lockdown_by("test-ld-owner", "panic", None, true).unwrap();
        assert!(is_owner_locked("test-ld-owner"));
        clear_lockdown("test-ld-owner");

        try_set_lockdown_by("test-ld-admin", "panic", None, false).unwrap();
        assert!(!is_owner_locked("test-ld-admin"));
        clear_lockdown("test-ld-admin");
        assert!(!is_owner_locked("test-ld-admin"), "no lockdown, nothing to protect");
    }

    /// Lockdowns saved before the owner flag existed still load, as not owner's.
    #[test]
    fn a_lockdown_saved_without_the_owner_flag_still_loads() {
        let old: LockdownState = serde_json::from_str(r#"{"reason":"panic","lockedAt":1,"expiresAt":null}"#).unwrap();
        assert!(!old.owner_locked);
        assert!(!serde_json::to_string(&old).unwrap().contains("ownerLocked"), "and isn't written out when false");
    }

    #[test]
    fn only_the_first_claim_starts_a_lockdown() {
        let g = "test-ld-claim";
        let first = try_set_lockdown(g, "raid", Some(now_ms() + 60_000));
        assert!(first.is_some());
        assert!(try_set_lockdown(g, "raid", Some(now_ms() + 60_000)).is_none());
        assert!(is_lockdown(g));
        clear_lockdown(g);
    }

    #[test]
    fn a_stale_timer_does_not_match_a_newer_lock() {
        let g = "test-ld-stale";
        let old = try_set_lockdown(g, "raid", Some(0)).unwrap();
        clear_lockdown(g);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let new = try_set_lockdown(g, "raid", Some(0)).unwrap();
        assert_ne!(old, new);
        assert!(!is_same_lock(g, old));
        assert!(is_same_lock(g, new));
        clear_lockdown(g);
    }

    #[test]
    fn lockdowns_are_per_guild() {
        try_set_lockdown("test-ld-a", "raid", None).unwrap();
        assert!(!is_lockdown("test-ld-b"));
        assert!(try_set_lockdown("test-ld-b", "panic", None).is_some());
        assert_eq!(lockdown_reason("test-ld-a").as_deref(), Some("raid"));
        assert_eq!(lockdown_reason("test-ld-b").as_deref(), Some("panic"));
        clear_lockdown("test-ld-a");
        clear_lockdown("test-ld-b");
    }
}
