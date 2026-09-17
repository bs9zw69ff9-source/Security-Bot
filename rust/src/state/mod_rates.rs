//! Mod rate-limit state (persisted to SQLite `mod_rates`).
//!
//! Scoped + persisted per guild, so a mod's limits in one server are
//! independent of - and survive restarts independently of - their activity in
//! any other.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{now_ms, CONFIG};
use crate::common::db;

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModEntry {
    pub bans: Vec<i64>,
    pub kicks: Vec<i64>,
    pub mutes: Vec<i64>,
    pub purges: Vec<i64>,
    pub lockdowns: Vec<i64>,
    pub warns: Vec<i64>,
}

impl ModEntry {
    fn slot(&mut self, action: &str) -> Option<&mut Vec<i64>> {
        Some(match action {
            "ban" => &mut self.bans,
            "kick" => &mut self.kicks,
            "mute" => &mut self.mutes,
            "purge" => &mut self.purges,
            "lockdown" => &mut self.lockdowns,
            "warn" => &mut self.warns,
            _ => return None,
        })
    }
}

pub fn limit_for(action: &str) -> usize {
    match action {
        "ban" => CONFIG.mod_ban_limit,
        "kick" => CONFIG.mod_kick_limit,
        "mute" => CONFIG.mod_mute_limit,
        "purge" => CONFIG.mod_purge_limit,
        "lockdown" => CONFIG.mod_lockdown_limit,
        "warn" => CONFIG.mod_warn_limit,
        _ => 0,
    }
}

type Rates = HashMap<String, HashMap<String, ModEntry>>;
static RATES: Lazy<Mutex<Rates>> = Lazy::new(|| Mutex::new(db::load_all("mod_rates")));

fn lock() -> std::sync::MutexGuard<'static, Rates> {
    match RATES.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

pub fn prune_window(arr: &[i64], window_ms: i64) -> Vec<i64> {
    let cutoff = now_ms() - window_ms;
    arr.iter().copied().filter(|t| *t > cutoff).collect()
}

pub struct LimitCheck {
    pub allowed: bool,
    pub used: usize,
    pub limit: usize,
    pub remaining: usize,
    pub resets_in_min: i64,
}

pub fn check_mod_limit(guild_id: &str, member_id: &str, action: &str) -> LimitCheck {
    let limit = limit_for(action);
    let mut map = lock();
    let entry = map.entry(guild_id.to_string()).or_default().entry(member_id.to_string()).or_default();
    let Some(slot) = entry.slot(action) else {
        return LimitCheck { allowed: true, used: 0, limit, remaining: limit, resets_in_min: 0 };
    };
    *slot = prune_window(slot, CONFIG.mod_window_ms);
    let used = slot.len();
    let allowed = used < limit;
    let resets_in_min = if !allowed {
        slot.first()
            .map(|oldest| ((*oldest + CONFIG.mod_window_ms - now_ms()) as f64 / 60_000.0).ceil() as i64)
            .unwrap_or(0)
    } else {
        0
    };
    LimitCheck { allowed, used, limit, remaining: limit.saturating_sub(used), resets_in_min }
}

pub fn record_mod_action(guild_id: &str, member_id: &str, action: &str) {
    let mut map = lock();
    let guild = map.entry(guild_id.to_string()).or_default();
    let entry = guild.entry(member_id.to_string()).or_default();
    if let Some(slot) = entry.slot(action) {
        *slot = prune_window(slot, CONFIG.mod_window_ms);
        slot.push(now_ms());
    }
    let snapshot = guild.clone();
    drop(map);
    db::put("mod_rates", guild_id, &snapshot);
}
