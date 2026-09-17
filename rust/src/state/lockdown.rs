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

pub fn locked_count() -> usize {
    lock().len()
}

/// Snapshot of every active lockdown, for boot recovery.
pub fn all() -> Vec<(String, LockdownState)> {
    lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

pub fn set_lockdown(guild_id: &str, reason: &str, expires_at: Option<i64>) {
    set_lockdown_with_changes(guild_id, reason, expires_at, Vec::new());
}

pub fn set_lockdown_with_changes(
    guild_id: &str,
    reason: &str,
    expires_at: Option<i64>,
    changed: Vec<LockedChannel>,
) {
    let state = LockdownState { reason: reason.to_string(), locked_at: now_ms(), expires_at, changed };
    lock().insert(guild_id.to_string(), state.clone());
    db::put("lockdown_state", guild_id, &state);
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
