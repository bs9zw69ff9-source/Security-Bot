//! Anti-Raid: join-velocity detection with a timed lockdown, plus optional
//! quarantine (kick) of brand-new accounts joining during that lockdown.

use once_cell::sync::Lazy;
use serenity::client::Context;
use serenity::model::guild::Member;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{now_ms, CONFIG};
use crate::common::embeds::{alert_owner, colors, sec_log};
use crate::common::permissions::try_dm;
use crate::state::lockdown::{self, is_lockdown, set_lockdown};
use super::mute::{lift_lockdown_channels, lock_all_text_channels};

/// guild id -> recent join timestamps
pub static JOIN_TRACKER: Lazy<Mutex<HashMap<String, Vec<i64>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Vec<i64>>> {
    match JOIN_TRACKER.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

pub async fn on_member_join(ctx: &Context, member: &Member) {
    let now = now_ms();
    let guild_id = member.guild_id;
    let gid = guild_id.to_string();

    // Quarantine brand-new accounts that join while THIS guild's raid lockdown
    // is active.
    if is_lockdown(&gid) && CONFIG.raid_kick_new_on_lock && !member.user.bot {
        let created_ms = member.user.id.created_at().unix_timestamp() * 1000;
        let age_min = (now - created_ms) as f64 / 60_000.0;
        if age_min < CONFIG.raid_min_account_age_min as f64 {
            try_dm(
                &ctx.http,
                member.user.id,
                "The server's in a temporary raid lockdown right now, so I couldn't let you in. Please try joining again a little later.",
            )
            .await;
            let _ = guild_id
                .kick_with_reason(&ctx.http, member.user.id, &format!("Raid lockdown: new account ({}m old)", age_min.round()))
                .await;
            sec_log(
                ctx,
                guild_id,
                "Raid Quarantine",
                &format!(
                    "Turned away <@{}> during the lockdown - it's a brand-new account ({}m old).",
                    member.user.id,
                    age_min.round()
                ),
                colors::DANGER,
            )
            .await;
            return;
        }
    }

    let recent = {
        let mut map = lock();
        let joins = map.entry(gid.clone()).or_default();
        joins.retain(|t| now - *t < CONFIG.raid_window_ms);
        joins.push(now);
        joins.len()
    };

    if recent >= CONFIG.raid_join_threshold && !is_lockdown(&gid) {
        let expires_at = now_ms() + CONFIG.raid_lockdown_min * 60_000;
        set_lockdown(&gid, "raid", Some(expires_at));
        alert_owner(
            ctx,
            guild_id,
            &format!(
                "Looks like a raid - **{recent}** people joined in just {}s. I've locked the server down for **{} min** to be safe.",
                CONFIG.raid_window_ms / 1000,
                CONFIG.raid_lockdown_min
            ),
            colors::NUKE,
            "Raid Detected",
        )
        .await;
        let outcome = lock_all_text_channels(ctx, guild_id).await;
        lockdown::record_changes(&gid, outcome.changes);

        let ctx2 = ctx.clone();
        let delay = std::time::Duration::from_millis((expires_at - now_ms()).max(0) as u64);
        let note = format!(
            "Lifted the raid lockdown automatically after **{} minutes**. Things should be back to normal.",
            CONFIG.raid_lockdown_min
        );
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            lift_lockdown_channels(&ctx2, guild_id, &note).await;
        });
    }
}

pub fn sweep() {
    let now = now_ms();
    let mut map = lock();
    map.retain(|_, arr| {
        arr.retain(|t| now - *t < CONFIG.raid_window_ms);
        !arr.is_empty()
    });
}
