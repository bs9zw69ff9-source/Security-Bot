//! Anti-Nuke engine.
//!
//! Scoped per guild - a user's actions in one server never count toward
//! thresholds in another. Fires once per real audit-log entry with a reliable
//! executor; bot-executed actions (i.e. our own commands) are skipped so
//! command paths remain the single counter for command-driven floods.

use once_cell::sync::Lazy;
use serenity::client::Context;
use serenity::model::guild::audit_log::{Action, AuditLogEntry, ChannelAction, EmojiAction, MemberAction, RoleAction, StickerAction, WebhookAction};
use serenity::model::id::{GuildId, RoleId, UserId};
use serenity::model::Permissions;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{now_ms, CONFIG, DANGER_PERMS};
use crate::common::embeds::{alert_owner, colors, sec_log};
use crate::common::guildinfo::{fetch_member, GuildInfo};
use crate::common::permissions::is_whitelisted;
use crate::state::lockdown::set_lockdown;
use super::mute::lock_all_text_channels;

/// "gid:uid" -> per-action-kind timestamp lists
type NukeCounts = HashMap<String, HashMap<String, Vec<i64>>>;
pub static NUKE_TRACKER: Lazy<Mutex<NukeCounts>> = Lazy::new(|| Mutex::new(HashMap::new()));
/// guild id -> nuke-response timestamps
pub static NUKE_STORM_TRACKER: Lazy<Mutex<HashMap<String, Vec<i64>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn nuke_lock() -> std::sync::MutexGuard<'static, NukeCounts> {
    match NUKE_TRACKER.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}
fn storm_lock() -> std::sync::MutexGuard<'static, HashMap<String, Vec<i64>>> {
    match NUKE_STORM_TRACKER.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// Push a timestamp under `key`; returns true if the threshold is reached.
pub fn bump(guild_id: &str, user_id: &str, key: &str, threshold: usize) -> bool {
    let now = now_ms();
    let mut map = nuke_lock();
    let entry = map.entry(format!("{guild_id}:{user_id}")).or_default();
    let arr = entry.entry(key.to_string()).or_default();
    arr.retain(|t| now - *t < CONFIG.nuke_window_ms);
    arr.push(now);
    arr.len() >= threshold
}

pub fn reset_bump(guild_id: &str, user_id: &str, key: &str) {
    let mut map = nuke_lock();
    if let Some(entry) = map.get_mut(&format!("{guild_id}:{user_id}")) {
        entry.insert(key.to_string(), Vec::new());
    }
}

/// Shared counter fed by EVERY destructive action, on top of the per-category
/// one. Without it a nuke that spreads itself across categories (a couple of
/// channel deletes, a couple of bans, a couple of webhooks) stays under every
/// individual threshold and never trips anything - the per-category limits sum
/// to ~24 free actions. This caps the total regardless of the mix.
const TOTAL_KEY: &str = "allDestructive";

/// Which counter actually crossed its line, so the alert can name the real reason.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Trip {
    Category,
    Total,
}

/// Feeds both the per-category counter and the shared one. Returns `None` when
/// nothing tripped.
pub fn bump_destructive(guild_id: &str, user_id: &str, key: &str, threshold: usize) -> Option<Trip> {
    let over_category = bump(guild_id, user_id, key, threshold);
    // A total threshold of 0 disables the aggregate check entirely.
    let over_total = CONFIG.nuke_total_threshold > 0 && bump(guild_id, user_id, TOTAL_KEY, CONFIG.nuke_total_threshold);
    if !over_category && !over_total {
        return None;
    }
    reset_bump(guild_id, user_id, key);
    reset_bump(guild_id, user_id, TOTAL_KEY);
    Some(if over_category { Trip::Category } else { Trip::Total })
}

pub fn total_reason() -> String {
    format!(
        "{}+ destructive actions in {}s",
        CONFIG.nuke_total_threshold,
        CONFIG.nuke_window_ms / 1000
    )
}

/// Push a timestamp for this guild's nuke-storm tracker; returns true once the
/// per-guild threshold is reached (and resets that guild's counter).
pub fn bump_storm(guild_id: &str) -> bool {
    let now = now_ms();
    let mut map = storm_lock();
    let arr = map.entry(guild_id.to_string()).or_default();
    arr.retain(|t| now - *t < CONFIG.nuke_storm_window_ms);
    arr.push(now);
    if arr.len() >= CONFIG.nuke_storm_threshold {
        arr.clear();
        return true;
    }
    false
}

pub fn sweep() {
    let now = now_ms();
    let mut map = nuke_lock();
    map.retain(|_, entry| {
        entry.retain(|_, arr| {
            arr.retain(|t| now - *t < CONFIG.nuke_window_ms);
            !arr.is_empty()
        });
        !entry.is_empty()
    });
    drop(map);
    let mut storm = storm_lock();
    storm.retain(|_, arr| {
        arr.retain(|t| now - *t < CONFIG.nuke_storm_window_ms);
        !arr.is_empty()
    });
}

/// Several nuke responses in one guild in a short window ⇒ lock it down and
/// pull dangerous roles from everyone who isn't whitelisted.
pub async fn server_emergency_lock(ctx: &Context, guild_id: GuildId, reason: &str) {
    alert_owner(
        ctx,
        guild_id,
        &format!(
            "This is getting serious - {reason}. I'm putting the whole server into emergency lockdown: pulling dangerous roles from everyone who isn't whitelisted and locking every channel."
        ),
        colors::NUKE,
        "Emergency Lockdown",
    )
    .await;

    if let Some(info) = GuildInfo::from_cache(ctx, guild_id) {
        let members = guild_id.members(&ctx.http, None, None).await.unwrap_or_default();
        for m in members {
            if m.user.bot || is_whitelisted(&m, info.owner_id) {
                continue;
            }
            let danger = info.dangerous_editable_roles(&m.roles);
            if !danger.is_empty() {
                let _ = m.remove_roles(&ctx.http, &danger).await;
            }
        }
    }
    lock_all_text_channels(ctx, guild_id).await;
    set_lockdown(&guild_id.to_string(), "nukestorm", None);
}

/// Strip the executor's dangerous roles, then ban (falling back to kick, then
/// to leaving them de-permed with a loud alert).
pub async fn nuke_response(ctx: &Context, guild_id: GuildId, user_id: UserId, reason: &str) {
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else { return };
    let Some(member) = fetch_member(ctx, guild_id, user_id).await else { return };
    // Re-guard: never punish owner/whitelisted, even if reached here.
    if is_whitelisted(&member, info.owner_id) {
        return;
    }

    alert_owner(
        ctx,
        guild_id,
        &format!(
            "Anti-nuke just kicked in on <@{user_id}> (`{user_id}`).\n**What set it off:** {reason}\n**What I did:** pulled their dangerous roles and moved to ban them."
        ),
        colors::NUKE,
        "Anti-Nuke Triggered",
    )
    .await;

    let to_remove = info.dangerous_editable_roles(&member.roles);
    if !to_remove.is_empty() {
        if let Err(e) = member.remove_roles(&ctx.http, &to_remove).await {
            sec_log(
                ctx,
                guild_id,
                "Anti-Nuke",
                &format!("I couldn't pull the roles off <@{user_id}>: {e}"),
                colors::WARN,
            )
            .await;
        }
    }

    match guild_id.ban_with_reason(&ctx.http, user_id, 0, &format!("Anti-Nuke: {reason}")).await {
        Ok(()) => {
            sec_log(ctx, guild_id, "Anti-Nuke", &format!("Banned <@{user_id}> - {reason}"), colors::NUKE).await;
        }
        Err(e) => {
            // Ban failed (likely above the bot). Try kick; otherwise leave
            // de-permed + escalate.
            let kicked = guild_id.kick_with_reason(&ctx.http, user_id, &format!("Anti-Nuke: {reason}")).await.is_ok();
            alert_owner(
                ctx,
                guild_id,
                &format!(
                    "I couldn't ban <@{user_id}> ({e}). {}",
                    if kicked {
                        "I kicked them instead."
                    } else {
                        "The kick didn't go through either, so I've only managed to strip their roles. **Please check my role position right away.**"
                    }
                ),
                colors::DANGER,
                "Anti-Nuke Needs a Look",
            )
            .await;
        }
    }

    if bump_storm(&guild_id.to_string()) {
        server_emergency_lock(
            ctx,
            guild_id,
            &format!(
                "{}+ nuke responses within {}s",
                CONFIG.nuke_storm_threshold,
                CONFIG.nuke_storm_window_ms / 1000
            ),
        )
        .await;
    }
}

/// One audit-log entry, dispatched to the matching detector.
pub async fn on_audit_log_entry(ctx: &Context, entry: &AuditLogEntry, guild_id: GuildId) {
    let executor_id = match entry.user_id {
        id if id.get() == 0 => return,
        id => id,
    };
    if executor_id == ctx.cache.current_user().id {
        return;
    }
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else { return };
    let Some(executor) = fetch_member(ctx, guild_id, executor_id).await else { return };
    if is_whitelisted(&executor, info.owner_id) {
        return;
    }

    let gid = guild_id.to_string();
    let uid = executor_id.to_string();
    let win = CONFIG.nuke_window_ms / 1000;

    // Simple "N of the same action in the window" detectors.
    let simple: Option<(&str, usize, String)> = match entry.action {
        Action::Channel(ChannelAction::Delete) => Some((
            "chDel",
            CONFIG.nuke_channel_threshold,
            format!("Deleted {}+ channels in {win}s", CONFIG.nuke_channel_threshold),
        )),
        Action::Channel(ChannelAction::Create) => Some((
            "chCreate",
            CONFIG.nuke_channel_create_thresh,
            format!("Created {}+ channels in {win}s", CONFIG.nuke_channel_create_thresh),
        )),
        Action::Role(RoleAction::Delete) => Some((
            "roleDel",
            CONFIG.nuke_role_threshold,
            format!("Deleted {}+ roles in {win}s", CONFIG.nuke_role_threshold),
        )),
        Action::Role(RoleAction::Create) => Some((
            "roleCreate",
            CONFIG.nuke_role_create_thresh,
            format!("Created {}+ roles in {win}s", CONFIG.nuke_role_create_thresh),
        )),
        Action::Member(MemberAction::BanAdd) => Some((
            "bans",
            CONFIG.nuke_ban_threshold,
            format!("Issued {}+ bans in {win}s", CONFIG.nuke_ban_threshold),
        )),
        Action::Member(MemberAction::Kick) | Action::Member(MemberAction::Prune) => Some((
            "kicks",
            CONFIG.nuke_kick_threshold,
            format!("Removed {}+ members in {win}s", CONFIG.nuke_kick_threshold),
        )),
        Action::Emoji(EmojiAction::Delete) | Action::Sticker(StickerAction::Delete) => Some((
            "emojiDel",
            CONFIG.nuke_emoji_threshold,
            format!("Deleted {}+ emojis/stickers in {win}s", CONFIG.nuke_emoji_threshold),
        )),
        _ => None,
    };
    if let Some((key, threshold, reason)) = simple {
        if let Some(trip) = bump_destructive(&gid, &uid, key, threshold) {
            let reason = if trip == Trip::Category { reason } else { total_reason() };
            nuke_response(ctx, guild_id, executor_id, &reason).await;
        }
        return;
    }

    match entry.action {
        Action::Webhook(WebhookAction::Create) => {
            if let Some(trip) = bump_destructive(&gid, &uid, "webhooks", CONFIG.nuke_webhook_threshold) {
                // Clean up whatever this user's webhooks were, best effort.
                if let Ok(channels) = guild_id.channels(&ctx.http).await {
                    for (cid, _) in channels {
                        if let Ok(hooks) = cid.webhooks(&ctx.http).await {
                            for wh in hooks.iter().filter(|w| w.user.as_ref().map(|u| u.id) == Some(executor_id)) {
                                let _ = wh.delete(&ctx.http).await;
                            }
                        }
                    }
                }
                let reason = if trip == Trip::Category {
                    format!("Created {}+ webhooks in {win}s", CONFIG.nuke_webhook_threshold)
                } else {
                    total_reason()
                };
                nuke_response(ctx, guild_id, executor_id, &reason).await;
            }
        }

        Action::Role(RoleAction::Update) => {
            let Some(changes) = entry.changes.as_ref() else { return };
            let Some((old_p, new_p)) = permission_change(changes) else { return };
            let escalated = DANGER_PERMS.iter().any(|p| !old_p.contains(*p) && new_p.contains(*p));
            if !escalated {
                return;
            }
            let target_id = entry.target_id.map(|t| t.get()).unwrap_or(0);
            let role = RoleId::new(target_id.max(1));
            if target_id != 0 && info.role_editable(role) {
                let _ = guild_id.edit_role(&ctx.http, role, serenity::builder::EditRole::new().permissions(old_p)).await;
            }
            alert_owner(
                ctx,
                guild_id,
                &format!("<@{executor_id}> just handed <@&{target_id}> some dangerous permissions. I've rolled that back."),
                colors::WARN,
                "Permission Change Reverted",
            )
            .await;
            if let Some(trip) = bump_destructive(&gid, &uid, "permEsc", 3) {
                let reason = if trip == Trip::Category {
                    "Repeated permission escalation".to_string()
                } else {
                    total_reason()
                };
                nuke_response(ctx, guild_id, executor_id, &reason).await;
            }
        }

        Action::Member(MemberAction::BotAdd) => {
            let target_id = entry.target_id.map(|t| t.get()).unwrap_or(0);
            if CONFIG.nuke_bot_add_action == "kick" && target_id != 0 {
                let _ = guild_id
                    .kick_with_reason(&ctx.http, UserId::new(target_id), "Anti-nuke: unauthorized bot add")
                    .await;
            }

            // Strip EVERY removable role from whoever added the bot.
            // (Skips @everyone, managed/integration roles, and anything above
            // my top role.)
            let removable: Vec<RoleId> = executor
                .roles
                .iter()
                .filter(|r| r.get() != guild_id.get() && info.role_editable(**r))
                .copied()
                .collect();
            let unstrippable: Vec<RoleId> = executor
                .roles
                .iter()
                .filter(|r| r.get() != guild_id.get() && !info.role_editable(**r))
                .copied()
                .collect();
            if !removable.is_empty() {
                let _ = executor.remove_roles(&ctx.http, &removable).await;
            }

            let stripped_list = if removable.is_empty() {
                "none".to_string()
            } else {
                removable.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ")
            };
            let unstrippable_note = if unstrippable.is_empty() {
                String::new()
            } else {
                format!(
                    "\nCouldn't take these (managed or above me): {}",
                    unstrippable.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ")
                )
            };
            alert_owner(
                ctx,
                guild_id,
                &format!(
                    "<@{executor_id}> added the bot <@{target_id}> - {}\nI also pulled **{}** role{} off <@{executor_id}>: {stripped_list}{unstrippable_note}",
                    if CONFIG.nuke_bot_add_action == "kick" {
                        "I've kicked it back out."
                    } else {
                        "you'll want to review this."
                    },
                    removable.len(),
                    if removable.len() == 1 { "" } else { "s" }
                ),
                colors::DANGER,
                "Bot Added",
            )
            .await;
        }

        Action::GuildUpdate => {
            alert_owner(
                ctx,
                guild_id,
                &format!("<@{executor_id}> changed the server settings. Might be worth a glance at the audit log."),
                colors::WARN,
                "Server Settings Changed",
            )
            .await;
        }

        _ => {}
    }
}

/// Pull the old/new permission bitfields out of a RoleUpdate's change list.
fn permission_change(changes: &[serenity::model::guild::audit_log::Change]) -> Option<(Permissions, Permissions)> {
    use serenity::model::guild::audit_log::Change;
    changes.iter().find_map(|c| match c {
        Change::Permissions { old, new } => Some((old.unwrap_or_else(Permissions::empty), new.unwrap_or_else(Permissions::empty))),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gap this closes: rotating through categories used to keep every
    /// per-category counter under its own limit, so an attacker got the sum of
    /// all of them for free. The shared counter has to catch that.
    #[test]
    fn rotating_categories_trips_on_the_shared_counter() {
        let cats = [
            ("chDel", CONFIG.nuke_channel_threshold),
            ("chCreate", CONFIG.nuke_channel_create_thresh),
            ("roleDel", CONFIG.nuke_role_threshold),
            ("roleCreate", CONFIG.nuke_role_create_thresh),
            ("bans", CONFIG.nuke_ban_threshold),
            ("kicks", CONFIG.nuke_kick_threshold),
            ("webhooks", CONFIG.nuke_webhook_threshold),
            ("emojiDel", CONFIG.nuke_emoji_threshold),
        ];
        nuke_lock().clear();
        let mut landed = 0;
        let mut trip = None;
        for i in 0..100 {
            let (key, threshold) = cats[i % cats.len()];
            landed += 1;
            trip = bump_destructive("g", "rotating", key, threshold);
            if trip.is_some() {
                break;
            }
        }
        assert!(trip == Some(Trip::Total), "a rotating attack must trip the shared counter");
        assert_eq!(landed, CONFIG.nuke_total_threshold, "no more actions may land than the shared threshold allows");
    }

    /// Hammering one category still trips that category, not the shared one.
    #[test]
    fn single_category_burst_still_trips_its_own_counter() {
        nuke_lock().clear();
        let mut landed = 0;
        let mut trip = None;
        for _ in 0..100 {
            landed += 1;
            trip = bump_destructive("g", "burst", "chDel", CONFIG.nuke_channel_threshold);
            if trip.is_some() {
                break;
            }
        }
        assert!(trip.is_some());
        assert_eq!(landed, CONFIG.nuke_channel_threshold.min(CONFIG.nuke_total_threshold));
    }
}
