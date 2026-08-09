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
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::common::config::{now_ms, CONFIG, DANGER_PERMS};
use crate::common::embeds::{alert_owner, colors, sec_log};
use crate::common::guildinfo::{fetch_member, GuildInfo};
use crate::common::permissions::is_whitelisted;

/// "gid:uid" -> per-action-kind timestamp lists
type NukeCounts = HashMap<String, HashMap<String, Vec<i64>>>;
pub static NUKE_TRACKER: Lazy<Mutex<NukeCounts>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// "gid:uid" for every user with a nuke response currently running.
static RESPONDING: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));

fn nuke_lock() -> std::sync::MutexGuard<'static, NukeCounts> {
    match NUKE_TRACKER.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

fn responding_lock() -> std::sync::MutexGuard<'static, HashSet<String>> {
    match RESPONDING.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// True while a response for this user is already running.
///
/// Banning someone takes a few HTTP round trips, and the attacker's remaining
/// audit-log entries keep arriving throughout. Without this they refill the
/// counter and trip a second, third, fourth response for a user who is already
/// being dealt with: duplicate bans, duplicate alerts, duplicate role strips.
pub fn response_in_flight(guild_id: &str, user_id: &str) -> bool {
    responding_lock().contains(&format!("{guild_id}:{user_id}"))
}

/// Claim the right to respond. False means someone else already has it.
fn begin_response(guild_id: &str, user_id: &str) -> bool {
    responding_lock().insert(format!("{guild_id}:{user_id}"))
}

fn end_response(guild_id: &str, user_id: &str) {
    responding_lock().remove(&format!("{guild_id}:{user_id}"));
}

/// Drop expired timestamps, add one for now, and report whether the threshold
/// is met. Caller holds the lock.
fn push_and_check(entry: &mut HashMap<String, Vec<i64>>, key: &str, threshold: usize, now: i64) -> bool {
    let arr = entry.entry(key.to_string()).or_default();
    arr.retain(|t| now - *t < CONFIG.nuke_window_ms);
    arr.push(now);
    arr.len() >= threshold
}

/// Shared counter fed by EVERY destructive action, on top of the per-category
/// one. Without it a nuke that spreads itself across categories (a couple of
/// channel deletes, a couple of bans, a couple of webhooks) stays under every
/// individual threshold and never trips anything - the per-category limits sum
/// to ~24 free actions. This caps the total regardless of the mix.
const TOTAL_KEY: &str = "allDestructive";

/// Which counter actually crossed its line, so the alert can name the real reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trip {
    Category,
    Total,
}

/// Feeds both the per-category counter and the shared one. Returns `None` when
/// nothing tripped.
///
/// The push and the reset happen under **one** lock. Done as separate
/// `bump()` then `reset_bump()` calls there was a window in between where a
/// concurrently-arriving event could push, also see the threshold met, and
/// trip as well. A nuke bot firing actions in parallel produces exactly that
/// pattern, so one burst fired several overlapping responses.
pub fn bump_destructive(guild_id: &str, user_id: &str, key: &str, threshold: usize) -> Option<Trip> {
    let now = now_ms();
    let mut map = nuke_lock();
    let entry = map.entry(format!("{guild_id}:{user_id}")).or_default();

    let over_category = push_and_check(entry, key, threshold, now);
    // A total threshold of 0 disables the aggregate check entirely.
    let over_total = CONFIG.nuke_total_threshold > 0
        && push_and_check(entry, TOTAL_KEY, CONFIG.nuke_total_threshold, now);

    if !over_category && !over_total {
        return None;
    }
    entry.insert(key.to_string(), Vec::new());
    entry.insert(TOTAL_KEY.to_string(), Vec::new());
    Some(if over_category { Trip::Category } else { Trip::Total })
}

pub fn total_reason() -> String {
    format!(
        "{}+ destructive actions in {}s",
        CONFIG.nuke_total_threshold,
        CONFIG.nuke_window_ms / 1000
    )
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
}

/// Strip the executor's dangerous roles, then ban (falling back to kick, then
/// to leaving them de-permed with a loud alert).
pub async fn nuke_response(ctx: &Context, guild_id: GuildId, user_id: UserId, reason: &str) {
    let gid = guild_id.to_string();
    let uid = user_id.to_string();
    // One response per user at a time. Without this, entries still streaming in
    // from actions they already performed trip again and start a second run
    // while the first is mid-ban.
    if !begin_response(&gid, &uid) {
        return;
    }
    run_nuke_response(ctx, guild_id, user_id, reason).await;
    end_response(&gid, &uid);
}

async fn run_nuke_response(ctx: &Context, guild_id: GuildId, user_id: UserId, reason: &str) {
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else { return };
    let Some(member) = fetch_member(ctx, guild_id, user_id).await else { return };
    // Re-guard: never punish owner/whitelisted, even if reached here.
    if is_whitelisted(&member, info.owner_id) {
        return;
    }

    // Roles come off and the ban goes out BEFORE anyone is told about it.
    // Alerting first meant an owner DM plus a channel send, each a full HTTP
    // round trip, while the attacker carried on working. Stop the bleeding,
    // then narrate.
    let to_remove = info.dangerous_editable_roles(&member.roles);
    let strip_error = if to_remove.is_empty() {
        None
    } else {
        member.remove_roles(&ctx.http, &to_remove).await.err().map(|e| e.to_string())
    };

    let ban_result = guild_id.ban_with_reason(&ctx.http, user_id, 0, &format!("Anti-Nuke: {reason}")).await;
    // Only now that they are de-permed and banned does anything get written.
    let kicked = match &ban_result {
        Ok(()) => false,
        // Ban failed (likely above the bot). Try kick; otherwise leave
        // de-permed + escalate.
        Err(_) => guild_id.kick_with_reason(&ctx.http, user_id, &format!("Anti-Nuke: {reason}")).await.is_ok(),
    };

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

    if let Some(e) = strip_error {
        sec_log(
            ctx,
            guild_id,
            "Anti-Nuke",
            &format!("I couldn't pull the roles off <@{user_id}>: {e}"),
            colors::WARN,
        )
        .await;
    }

    match ban_result {
        Ok(()) => {
            sec_log(ctx, guild_id, "Anti-Nuke", &format!("Banned <@{user_id}> - {reason}"), colors::NUKE).await;
        }
        Err(e) => {
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

    let gid = guild_id.to_string();
    let uid = executor_id.to_string();
    let win = CONFIG.nuke_window_ms / 1000;

    // Already dealing with this one. Their remaining entries describe actions
    // that have already happened; counting them again only produces duplicate
    // responses.
    if response_in_flight(&gid, &uid) {
        return;
    }

    // Counting happens BEFORE any `.await`.
    //
    // This used to fetch the executor and check the whitelist first, which is
    // at minimum a cache lookup and at worst a full HTTP round trip. A nuke bot
    // firing in parallel gets every one of its entries parked in that await
    // together, so the counter did not start climbing until a round trip had
    // already passed and a pile of actions had landed. Bumping is a mutex
    // operation with no await in it, so the trip now fires on the earliest
    // entry that crosses the line.
    //
    // The whitelist check moves to the response path. A whitelisted user's
    // actions get counted, and are then discarded when the trip is evaluated,
    // which costs nothing: counters are per user.

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
            // Counted before the revert, for the same reason as everything
            // else: the revert and the alert are both round trips.
            let trip = bump_destructive(&gid, &uid, "permEsc", 3);

            let target_id = entry.target_id.map(|t| t.get()).unwrap_or(0);
            let role = RoleId::new(target_id.max(1));
            if target_id != 0 && info.role_editable(role) {
                let _ = guild_id.edit_role(&ctx.http, role, serenity::builder::EditRole::new().permissions(old_p)).await;
            }

            if let Some(trip) = trip {
                let reason = if trip == Trip::Category {
                    "Repeated permission escalation".to_string()
                } else {
                    total_reason()
                };
                nuke_response(ctx, guild_id, executor_id, &reason).await;
                return;
            }
            // Below the line: still worth telling the owner it happened.
            if !is_whitelisted_now(ctx, guild_id, executor_id, &info).await {
                alert_owner(
                    ctx,
                    guild_id,
                    &format!("<@{executor_id}> just handed <@&{target_id}> some dangerous permissions. I've rolled that back."),
                    colors::WARN,
                    "Permission Change Reverted",
                )
                .await;
            }
        }

        Action::Member(MemberAction::BotAdd) => {
            let Some(executor) = fetch_member(ctx, guild_id, executor_id).await else { return };
            if is_whitelisted(&executor, info.owner_id) {
                return;
            }
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
            if is_whitelisted_now(ctx, guild_id, executor_id, &info).await {
                return;
            }
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

/// Whitelist check for the paths that only alert, so a trusted admin doing
/// ordinary admin work doesn't generate noise.
async fn is_whitelisted_now(ctx: &Context, guild_id: GuildId, user_id: UserId, info: &GuildInfo) -> bool {
    match fetch_member(ctx, guild_id, user_id).await {
        Some(m) => is_whitelisted(&m, info.owner_id),
        None => false,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counters are global and tests run in parallel, so every test works
    /// against a user id nobody else touches rather than clearing the map.
    fn fresh_user() -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        format!("u{}", N.fetch_add(1, Ordering::SeqCst))
    }

    fn categories() -> Vec<(&'static str, usize)> {
        vec![
            ("chDel", CONFIG.nuke_channel_threshold),
            ("chCreate", CONFIG.nuke_channel_create_thresh),
            ("roleDel", CONFIG.nuke_role_threshold),
            ("roleCreate", CONFIG.nuke_role_create_thresh),
            ("bans", CONFIG.nuke_ban_threshold),
            ("kicks", CONFIG.nuke_kick_threshold),
            ("webhooks", CONFIG.nuke_webhook_threshold),
            ("emojiDel", CONFIG.nuke_emoji_threshold),
        ]
    }

    /// Rotating through categories used to keep every per-category counter
    /// under its own limit, so an attacker got the sum of all of them for free.
    /// The shared counter has to catch that.
    #[test]
    fn rotating_categories_trips_on_the_shared_counter() {
        let cats = categories();
        let user = fresh_user();
        let mut landed = 0;
        let mut trip = None;
        for i in 0..100 {
            let (key, threshold) = cats[i % cats.len()];
            landed += 1;
            trip = bump_destructive("g", &user, key, threshold);
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
        let user = fresh_user();
        let mut landed = 0;
        let mut trip = None;
        for _ in 0..100 {
            landed += 1;
            trip = bump_destructive("g", &user, "chDel", CONFIG.nuke_channel_threshold);
            if trip.is_some() {
                break;
            }
        }
        assert!(trip.is_some());
        assert_eq!(landed, CONFIG.nuke_channel_threshold.min(CONFIG.nuke_total_threshold));
    }

    /// The aggregate binds even when a category is effectively unlimited.
    #[test]
    fn the_shared_counter_binds_when_a_category_is_unlimited() {
        let user = fresh_user();
        let mut landed = 0;
        for _ in 0..100 {
            landed += 1;
            if let Some(trip) = bump_destructive("g", &user, "chDel", usize::MAX) {
                assert_eq!(trip, Trip::Total);
                break;
            }
        }
        assert_eq!(landed, CONFIG.nuke_total_threshold);
    }

    /// A nuke bot fires in parallel, so many events reach the counter at once.
    /// The old bump-then-reset pair released the lock in between, letting
    /// several threads all see the threshold met off the same actions. The
    /// number of trips must stay bounded by what the actions actually justify.
    #[test]
    fn a_concurrent_burst_cannot_trip_more_often_than_the_actions_justify() {
        const ACTIONS: usize = 16;
        let threshold = 3;

        for _ in 0..50 {
            let user = fresh_user();
            let trips = std::sync::Arc::new(AtomicUsize::new(0));
            let threads: Vec<_> = (0..ACTIONS)
                .map(|_| {
                    let trips = std::sync::Arc::clone(&trips);
                    let user = user.clone();
                    std::thread::spawn(move || {
                        if bump_destructive("g", &user, "chDel", threshold).is_some() {
                            trips.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }

            let n = trips.load(Ordering::SeqCst);
            assert!(n >= 1, "a {ACTIONS}-action burst must trip at least once");
            assert!(
                n <= ACTIONS / threshold,
                "tripped {n} times, more than {ACTIONS} actions over a threshold of {threshold} permit"
            );
        }
    }

    /// The single-flight guard is what turns "tripped more than once" into
    /// "responded once".
    #[test]
    fn only_one_response_runs_per_user_at_a_time() {
        let user = fresh_user();
        assert!(begin_response("g", &user), "first claim should win");
        assert!(!begin_response("g", &user), "second claim must be refused");
        assert!(response_in_flight("g", &user));

        // A different user in the same guild is unaffected, as is the same
        // user in a different guild.
        let other = fresh_user();
        assert!(begin_response("g", &other));
        assert!(begin_response("g2", &user));

        end_response("g", &user);
        assert!(!response_in_flight("g", &user));
        assert!(begin_response("g", &user), "claimable again once released");

        end_response("g", &user);
        end_response("g", &other);
        end_response("g2", &user);
    }

    /// Counters must not leak across guilds.
    #[test]
    fn counters_are_scoped_per_guild() {
        let user = fresh_user();
        for _ in 0..CONFIG.nuke_total_threshold.saturating_sub(1) {
            assert!(bump_destructive("guild-a", &user, "chDel", 99).is_none());
        }
        // The same user acting in a different guild starts from zero.
        assert!(
            bump_destructive("guild-b", &user, "chDel", 99).is_none(),
            "another guild's count must not carry over"
        );
    }
}
