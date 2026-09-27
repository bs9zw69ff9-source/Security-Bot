//! Anti-Raid: join-velocity detection with a timed lockdown, plus optional
//! quarantine (kick) of brand-new accounts joining during that lockdown.
//!
//! Every threshold comes from the joining guild's own settings, and the join
//! window is tracked per guild, so a raid on one server can't push another one
//! into lockdown.

use once_cell::sync::Lazy;
use serenity::client::Context;
use serenity::model::guild::Member;
use serenity::model::id::GuildId;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::now_ms;
use crate::common::embeds::{alert_owner, colors, sec_log};
use crate::common::permissions::try_dm;
use crate::state::guild_settings;
use crate::state::lockdown::{self, is_lockdown};
use super::mute::{lock_all_text_channels, schedule_lockdown_lift};

/// guild -> recent join timestamps
static JOIN_TRACKER: Lazy<Mutex<HashMap<GuildId, Vec<i64>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn lock() -> std::sync::MutexGuard<'static, HashMap<GuildId, Vec<i64>>> {
    JOIN_TRACKER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Count a join and return how many landed inside the window.
fn record_join(guild_id: GuildId, now: i64, window_ms: i64) -> usize {
    let mut map = lock();
    let joins = map.entry(guild_id).or_default();
    joins.retain(|t| now - *t < window_ms);
    joins.push(now);
    joins.len()
}

pub async fn on_member_join(ctx: &Context, member: &Member) {
    let now = now_ms();
    let guild_id = member.guild_id;
    let gid = guild_id.to_string();
    let cfg = guild_settings::raid(&gid);

    // Off for this server. Nothing below runs: no join is counted, so turning
    // it back on later starts from a clean window rather than firing
    // immediately on joins that happened while it was off.
    if !cfg.enabled {
        return;
    }

    // Quarantine brand-new accounts that join while THIS guild's raid lockdown
    // is active.
    if is_lockdown(&gid) && cfg.kick_new_accounts && !member.user.bot {
        let created_ms = member.user.id.created_at().unix_timestamp() * 1000;
        let age_min = (now - created_ms) as f64 / 60_000.0;
        if age_min < cfg.min_account_age_min as f64 {
            try_dm(
                &ctx.http,
                member.user.id,
                "The server's in a temporary raid lockdown right now, so I couldn't let you in. Please try joining again a little later.",
            )
            .await;
            let kicked = guild_id
                .kick_with_reason(&ctx.http, member.user.id, &format!("Raid lockdown: new account ({}m old)", age_min.round()))
                .await;
            let desc = match kicked {
                Ok(()) => format!(
                    "Turned away <@{}> during the lockdown - it's a brand-new account ({}m old).",
                    member.user.id,
                    age_min.round()
                ),
                Err(e) => format!(
                    "<@{}> joined during the lockdown on a brand-new account ({}m old), but I couldn't kick them: {e}",
                    member.user.id,
                    age_min.round()
                ),
            };
            sec_log(ctx, guild_id, "Raid Quarantine", &desc, colors::DANGER).await;
            return;
        }
    }

    let recent = record_join(guild_id, now, cfg.window_ms);
    if recent < cfg.join_threshold {
        return;
    }
    let expires_at = now + cfg.lockdown_min * 60_000;
    let Some(locked_at) = lockdown::try_set_lockdown(&gid, "raid", Some(expires_at)) else { return };

    alert_owner(
        ctx,
        guild_id,
        &format!(
            "Looks like a raid - **{recent}** people joined in just {}s. I've locked the server down for **{} min** to be safe.",
            cfg.window_ms / 1000,
            cfg.lockdown_min
        ),
        colors::NUKE,
        "Raid Detected",
    )
    .await;
    let outcome = lock_all_text_channels(ctx, guild_id).await;
    lockdown::record_changes(&gid, outcome.changes);

    schedule_lockdown_lift(
        ctx.clone(),
        guild_id,
        locked_at,
        expires_at - now_ms(),
        format!(
            "Lifted the raid lockdown automatically after **{} minutes**. Things should be back to normal.",
            cfg.lockdown_min
        ),
    );
}

/// Drop join timestamps that have aged out of their guild's window.
pub fn sweep() {
    let now = now_ms();
    let mut map = lock();
    map.retain(|gid, arr| {
        let window = guild_settings::raid(&gid.to_string()).window_ms;
        arr.retain(|t| now - *t < window);
        !arr.is_empty()
    });
}

/// Forget a guild's join window (the bot left it).
pub fn forget_guild(guild_id: GuildId) {
    lock().remove(&guild_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_are_counted_per_guild() {
        let (a, b) = (GuildId::new(9_000_001), GuildId::new(9_000_002));
        let now = 1_000_000;
        for i in 0..5 {
            record_join(a, now + i, 10_000);
        }
        assert_eq!(record_join(a, now + 5, 10_000), 6);
        assert_eq!(record_join(b, now + 5, 10_000), 1, "guild A's joins must not count toward guild B");
    }

    #[test]
    fn each_guild_uses_its_own_window() {
        let (a, b) = (GuildId::new(9_000_003), GuildId::new(9_000_004));
        record_join(a, 0, 1_000);
        record_join(b, 0, 60_000);
        // 5s later: outside A's 1s window, inside B's 60s one.
        assert_eq!(record_join(a, 5_000, 1_000), 1);
        assert_eq!(record_join(b, 5_000, 60_000), 2);
    }

    #[test]
    fn concurrent_joins_across_guilds_stay_separate() {
        let guilds: Vec<GuildId> = (0..8).map(|i| GuildId::new(9_100_000 + i)).collect();
        let threads: Vec<_> = guilds
            .iter()
            .copied()
            .map(|g| {
                std::thread::spawn(move || {
                    for i in 0..50 {
                        record_join(g, 1_000 + i, 1_000_000);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        for g in guilds {
            assert_eq!(lock().get(&g).map(|v| v.len()), Some(50));
        }
    }
}
