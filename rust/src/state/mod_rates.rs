//! Mod rate-limit state (persisted to SQLite `mod_rates`).
//!
//! Scoped + persisted per guild, so a mod's limits in one server are
//! independent of - and survive restarts independently of - their activity in
//! any other.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::now_ms;
use crate::state::guild_settings;
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
    let cfg = guild_settings::moderation(guild_id);
    let limit = cfg.limit_for(action);
    let window_ms = cfg.window_ms;
    let mut map = lock();
    let entry = map.entry(guild_id.to_string()).or_default().entry(member_id.to_string()).or_default();
    let Some(slot) = entry.slot(action) else {
        return LimitCheck { allowed: true, used: 0, limit, remaining: limit, resets_in_min: 0 };
    };
    *slot = prune_window(slot, window_ms);
    let used = slot.len();
    let allowed = used < limit;
    let resets_in_min = if !allowed {
        slot.first()
            .map(|oldest| ((*oldest + window_ms - now_ms()) as f64 / 60_000.0).ceil() as i64)
            .unwrap_or(0)
    } else {
        0
    };
    LimitCheck { allowed, used, limit, remaining: limit.saturating_sub(used), resets_in_min }
}

pub fn record_mod_action(guild_id: &str, member_id: &str, action: &str) {
    let window_ms = guild_settings::moderation(guild_id).window_ms;
    let mut map = lock();
    let guild = map.entry(guild_id.to_string()).or_default();
    let entry = guild.entry(member_id.to_string()).or_default();
    if let Some(slot) = entry.slot(action) {
        *slot = prune_window(slot, window_ms);
        slot.push(now_ms());
    }
    let snapshot = guild.clone();
    drop(map);
    db::put("mod_rates", guild_id, &snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tunables::Tunable;

    /// Same moderator id, two servers with different limits: each server
    /// counts and caps on its own.
    #[test]
    fn limits_and_counts_are_per_guild() {
        let (a, b, modr) = ("test-mr-a", "test-mr-b", "4242");
        guild_settings::update(a, |s| {
            s.thresholds.insert(Tunable::ModBanLimit.key().into(), 1);
        });
        guild_settings::update(b, |s| {
            s.thresholds.insert(Tunable::ModBanLimit.key().into(), 3);
        });

        assert!(check_mod_limit(a, modr, "ban").allowed);
        record_mod_action(a, modr, "ban");
        let in_a = check_mod_limit(a, modr, "ban");
        assert!(!in_a.allowed);
        assert_eq!(in_a.limit, 1);

        let in_b = check_mod_limit(b, modr, "ban");
        assert!(in_b.allowed, "guild A's ban must not count in guild B");
        assert_eq!(in_b.used, 0);
        assert_eq!(in_b.limit, 3);
    }
}
