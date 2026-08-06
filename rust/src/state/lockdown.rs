//! Lockdown state (persisted to SQLite `lockdown_state`).

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::now_ms;
use crate::common::db;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockdownState {
    /// "raid" | "panic" | "nukestorm" | "manual"
    pub reason: String,
    pub locked_at: i64,
    /// `None` for manual/panic/nukestorm locks, which never auto-expire.
    pub expires_at: Option<i64>,
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

pub fn locked_count() -> usize {
    lock().len()
}

/// Snapshot of every active lockdown, for boot recovery.
pub fn all() -> Vec<(String, LockdownState)> {
    lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

pub fn set_lockdown(guild_id: &str, reason: &str, expires_at: Option<i64>) {
    let state = LockdownState { reason: reason.to_string(), locked_at: now_ms(), expires_at };
    lock().insert(guild_id.to_string(), state.clone());
    db::put("lockdown_state", guild_id, &state);
}

pub fn clear_lockdown(guild_id: &str) {
    lock().remove(guild_id);
    db::delete("lockdown_state", guild_id);
}
