//! Mute / unmute utilities (strips + stashes roles, restores on unmute) and
//! the boot recovery that re-applies, reschedules, or expires them.

use serenity::client::Context;
use serenity::model::id::{GuildId, RoleId, UserId};
use serenity::model::Permissions;
use std::time::Duration;

use crate::common::config::now_ms;
use crate::common::embeds::{colors, sec_log};
use crate::common::guildinfo::GuildInfo;
use crate::state::guild_settings::gc;
use crate::state::lockdown::{self, LockedChannel, LockedTarget};
use crate::state::muted_roles::{self, MuteStash};

fn mention_roles(ids: &[String]) -> String {
    if ids.is_empty() {
        "none".to_string()
    } else {
        ids.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(", ")
    }
}

/// Apply the mute role, stashing every role we're able to strip so unmute can
/// hand them back. Returns false when the guild has no usable mute role.
pub async fn mute_user(
    ctx: &Context,
    info: &GuildInfo,
    member: &serenity::model::guild::Member,
    duration_min: i64,
    reason: &str,
) -> bool {
    let guild_id = info.id;
    let mute_role_id = gc(&guild_id.to_string()).mute_role_id;
    if mute_role_id.is_empty() {
        return false;
    }
    let Ok(raw) = mute_role_id.parse::<u64>() else { return false };
    let mute_role = RoleId::new(raw);
    if !info.roles.contains_key(&mute_role) {
        return false;
    }

    // Everything we can actually take off: not @everyone, not the mute role,
    // not managed, and below our own top role.
    let removable: Vec<RoleId> = member
        .roles
        .iter()
        .filter(|r| r.get() != guild_id.get() && **r != mute_role && info.role_editable(**r))
        .copied()
        .collect();
    let unstrippable: Vec<RoleId> = member
        .roles
        .iter()
        .filter(|r| r.get() != guild_id.get() && **r != mute_role && !info.role_editable(**r))
        .copied()
        .collect();
    let stripped_ids: Vec<String> = removable.iter().map(|r| r.to_string()).collect();

    if !removable.is_empty() {
        if let Err(e) = member.remove_roles(&ctx.http, &removable).await {
            eprintln!("⚠️ mute role op failed: {e}");
        }
    }
    if let Err(e) = member.add_role(&ctx.http, mute_role).await {
        eprintln!("⚠️ mute role op failed: {e}");
    }

    // Merge with any roles stashed by an earlier mute so nothing is lost.
    let prior = muted_roles::get(&guild_id.to_string(), &member.user.id.to_string())
        .map(|s| s.roles)
        .unwrap_or_default();
    let mut roles = prior;
    for id in stripped_ids {
        if !roles.contains(&id) {
            roles.push(id);
        }
    }
    let stash_len = roles.len();
    let stash_list = mention_roles(&roles);
    muted_roles::set(
        &guild_id.to_string(),
        &member.user.id.to_string(),
        MuteStash {
            roles,
            reason: reason.to_string(),
            muted_at: now_ms(),
            expires_at: if duration_min > 0 { Some(now_ms() + duration_min * 60_000) } else { None },
        },
    );

    let unstrippable_note = if unstrippable.is_empty() {
        String::new()
    } else {
        format!(
            "\nCouldn't take these (managed or above me): {}",
            unstrippable.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ")
        )
    };
    sec_log(
        ctx,
        guild_id,
        "Member Muted",
        &format!(
            "<@{}> was muted for **{}** - {reason}\nI set aside **{stash_len}** role{} to give back on unmute: {stash_list}{unstrippable_note}",
            member.user.id,
            if duration_min > 0 { format!("{duration_min} min") } else { "as long as it takes".to_string() },
            if stash_len == 1 { "" } else { "s" }
        ),
        colors::MUTED,
    )
    .await;

    if duration_min > 0 {
        schedule_unmute(ctx.clone(), guild_id, member.user.id, duration_min * 60_000, "Auto-unmute (timer)");
    }
    true
}

/// Remove the mute role and restore whatever we stashed.
pub async fn unmute_user(ctx: &Context, guild_id: GuildId, user_id: UserId, reason: &str) {
    let mute_role_id = gc(&guild_id.to_string()).mute_role_id;
    let mute_role = mute_role_id.parse::<u64>().ok().map(RoleId::new);
    let member = guild_id.member(&ctx.http, user_id).await.ok();
    let stash = muted_roles::get(&guild_id.to_string(), &user_id.to_string());

    if let Some(member) = member {
        if let Some(mr) = mute_role {
            if member.roles.contains(&mr) {
                let _ = member.remove_role(&ctx.http, mr).await;
            }
        }

        let stash_roles = stash.as_ref().map(|s| s.roles.clone()).unwrap_or_default();
        if !stash_roles.is_empty() {
            let info = GuildInfo::from_cache(ctx, guild_id);
            let mut restorable: Vec<RoleId> = Vec::new();
            let mut lost: Vec<String> = Vec::new();
            for id in &stash_roles {
                let parsed = id.parse::<u64>().ok().map(RoleId::new);
                let ok = match (&info, parsed) {
                    (Some(i), Some(r)) => i.role_editable(r),
                    _ => false,
                };
                match (ok, parsed) {
                    (true, Some(r)) => restorable.push(r),
                    _ => lost.push(id.clone()),
                }
            }
            if !restorable.is_empty() {
                let _ = member.add_roles(&ctx.http, &restorable).await;
            }
            let restored_list = mention_roles(&restorable.iter().map(|r| r.to_string()).collect::<Vec<_>>());
            let lost_note = if lost.is_empty() {
                String::new()
            } else {
                format!("\nCouldn't restore these (deleted or above me): {}", mention_roles(&lost))
            };
            sec_log(
                ctx,
                guild_id,
                "Roles Restored",
                &format!(
                    "<@{user_id}> is unmuted, and I gave back **{}** role{}: {restored_list}{lost_note}\n_({reason})_",
                    restorable.len(),
                    if restorable.len() == 1 { "" } else { "s" }
                ),
                colors::SUCCESS,
            )
            .await;
        } else {
            sec_log(
                ctx,
                guild_id,
                "Member Unmuted",
                &format!("<@{user_id}> is unmuted. There were no stashed roles to give back. _({reason})_"),
                colors::SUCCESS,
            )
            .await;
        }
    }

    muted_roles::remove(&guild_id.to_string(), &user_id.to_string());
}

/// `setTimeout` in JS silently overflows past ~24.8 days; tokio sleeps take a
/// real Duration, so a long mute just sleeps the whole span in one task.
pub fn schedule_unmute(ctx: Context, guild_id: GuildId, user_id: UserId, delay_ms: i64, reason: &'static str) {
    let delay = Duration::from_millis(delay_ms.max(0) as u64);
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        unmute_user(&ctx, guild_id, user_id, reason).await;
    });
}

/// Boot recovery: re-apply the mute role if it was lost during downtime, then
/// reschedule or immediately expire each timed mute.
pub async fn recover_mutes(ctx: &Context) {
    for (guild_id_str, users) in muted_roles::all() {
        let Ok(raw) = guild_id_str.parse::<u64>() else { continue };
        let guild_id = GuildId::new(raw);
        if ctx.cache.guild(guild_id).is_none() {
            continue;
        }
        let mute_role = gc(&guild_id_str).mute_role_id.parse::<u64>().ok().map(RoleId::new);

        for (user_id_str, data) in users {
            let Ok(uid) = user_id_str.parse::<u64>() else { continue };
            let user_id = UserId::new(uid);

            if let Some(mr) = mute_role {
                let still_muted = data.expires_at.map(|e| e > now_ms()).unwrap_or(true);
                if still_muted {
                    if let Ok(m) = guild_id.member(&ctx.http, user_id).await {
                        if !m.roles.contains(&mr) {
                            let _ = m.add_role(&ctx.http, mr).await;
                        }
                    }
                }
            }

            let Some(expires_at) = data.expires_at else { continue }; // permanent - leave for manual /unmute
            let remaining = expires_at - now_ms();
            if remaining <= 0 {
                unmute_user(ctx, guild_id, user_id, "Auto-unmute (expired during downtime)").await;
            } else {
                schedule_unmute(ctx.clone(), guild_id, user_id, remaining, "Auto-unmute (timer, resumed post-restart)");
            }
        }
    }
}

/// Set (or clear) the Send Messages overwrite for one role on one channel,
/// preserving every other bit already on that overwrite.
///
/// `lock = Some(false)` denies, `lock = None` clears the bit entirely - the
/// equivalent of discord.js's `{ SendMessages: false }` / `{ SendMessages: null }`,
/// which only ever touch that single permission.
pub async fn set_send_messages(
    ctx: &Context,
    channel: &serenity::model::channel::GuildChannel,
    role: RoleId,
    lock: Option<bool>,
) -> bool {
    use serenity::model::channel::{PermissionOverwrite, PermissionOverwriteType};
    use serenity::model::Permissions;

    let existing = channel
        .permission_overwrites
        .iter()
        .find(|o| matches!(o.kind, PermissionOverwriteType::Role(r) if r == role));
    let (mut allow, mut deny) = existing.map(|o| (o.allow, o.deny)).unwrap_or((Permissions::empty(), Permissions::empty()));

    match lock {
        Some(false) => {
            allow.remove(Permissions::SEND_MESSAGES);
            deny.insert(Permissions::SEND_MESSAGES);
        }
        Some(true) => {
            deny.remove(Permissions::SEND_MESSAGES);
            allow.insert(Permissions::SEND_MESSAGES);
        }
        None => {
            allow.remove(Permissions::SEND_MESSAGES);
            deny.remove(Permissions::SEND_MESSAGES);
        }
    }

    channel
        .id
        .create_permission(&ctx.http, PermissionOverwrite { allow, deny, kind: PermissionOverwriteType::Role(role) })
        .await
        .is_ok()
}

/// Everything a lockdown takes away.
///
/// Denying Send Messages alone is not "nobody can type": threads have their own
/// permission, so every existing thread stays writable, and anyone able to
/// start a new thread can talk in it. All four go together.
///
/// Members with Administrator still bypass channel overwrites entirely. That is
/// Discord's own rule and it is what keeps the owner able to act during a lock.
fn lock_perms() -> serenity::model::Permissions {
    use serenity::model::Permissions;
    Permissions::SEND_MESSAGES
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::CREATE_PRIVATE_THREADS
}

/// Channels a lockdown covers. Forums hold posts, and voice/stage channels have
/// their own built-in text chat, so leaving any of them out leaves somewhere to
/// talk. Threads are children of these and inherit the thread permissions above.
fn is_text_like(ch: &serenity::model::channel::GuildChannel) -> bool {
    use serenity::model::channel::ChannelType;
    matches!(ch.kind, ChannelType::Text | ChannelType::News | ChannelType::Forum | ChannelType::Voice | ChannelType::Stage)
}

/// Reopen every text channel and clear lockdown state for a guild (shared by
/// the raid auto-lift timer, /panic unlock, and boot recovery of an expired
/// lock).
pub async fn lift_lockdown_channels(ctx: &Context, guild_id: GuildId, note: &str) {
    // Restores only what the lock changed, so channels that were read-only
    // before it started stay read-only after it lifts.
    unlock_all_text_channels(ctx, guild_id).await;
    lockdown::clear_lockdown(&guild_id.to_string());
    sec_log(ctx, guild_id, "Lockdown Lifted", note, colors::SUCCESS).await;
}

/// Boot recovery: reschedule / expire raid lockdowns; leave every other kind
/// active (they have no auto-expiry, same as before a restart).
pub async fn recover_lockdowns(ctx: &Context) {
    for (guild_id_str, state) in lockdown::all() {
        let Ok(raw) = guild_id_str.parse::<u64>() else { continue };
        let guild_id = GuildId::new(raw);
        if ctx.cache.guild(guild_id).is_none() {
            continue;
        }
        let Some(expires_at) = state.expires_at else { continue }; // manual - stays locked
        let remaining = expires_at - now_ms();
        if remaining <= 0 {
            lift_lockdown_channels(ctx, guild_id, "Auto-lifted (timer expired during downtime).").await;
        } else {
            let ctx2 = ctx.clone();
            let delay = Duration::from_millis(remaining as u64);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                lift_lockdown_channels(&ctx2, guild_id, "Auto-lifted (timer, resumed post-restart).").await;
            });
        }
    }
}

/// What locking one overwrite changes.
///
/// `None` means it is already fully locked, and that is the whole mechanism
/// behind "only reopen what was open": a channel that was read-only before the
/// lockdown produces no plan, so it never enters the record, so the lift never
/// touches it.
struct LockPlan {
    allow: Permissions,
    deny: Permissions,
    newly_denied: Permissions,
    allow_removed: Permissions,
}

fn plan_lock(allow: Permissions, deny: Permissions, perms: Permissions) -> Option<LockPlan> {
    let newly_denied = perms - deny;
    let allow_removed = perms & allow;
    if newly_denied.is_empty() && allow_removed.is_empty() {
        return None;
    }
    Some(LockPlan { allow: allow - perms, deny: deny | perms, newly_denied, allow_removed })
}

/// What lifting puts back: only the bits this lockdown itself changed. A deny
/// that predates the lockdown, and anything an admin altered while it was
/// active, are both left alone.
fn plan_unlock(
    allow: Permissions,
    deny: Permissions,
    denied: Permissions,
    allow_removed: Permissions,
    perms: Permissions,
) -> (Permissions, Permissions) {
    (allow | (allow_removed & perms), deny - (denied & perms))
}

/// The result of locking a guild down: how many channels changed, and exactly
/// what was changed so it can be put back.
pub struct LockOutcome {
    pub locked: usize,
    pub changes: Vec<LockedChannel>,
}

/// Lock every channel people can talk in. Shared by the raid responder and
/// /panic.
///
/// Two things happen per channel. @everyone gets the talking permissions
/// denied, and any *other* overwrite that explicitly allows one of them has
/// that allow removed. The second half is what makes the lock real: an explicit
/// allow on a role overwrite beats @everyone's deny, so without it every role
/// with a Send Messages allow talked straight through the lockdown.
///
/// A channel already fully locked contributes no changes, so it is not recorded
/// and a later lift leaves it exactly as it was.
pub async fn lock_all_text_channels(ctx: &Context, guild_id: GuildId) -> LockOutcome {
    use serenity::model::channel::{PermissionOverwrite, PermissionOverwriteType};

    let channels = guild_id.channels(&ctx.http).await.unwrap_or_default();
    let everyone = RoleId::new(guild_id.get());
    let perms = lock_perms();
    let mut outcome = LockOutcome { locked: 0, changes: Vec::new() };

    for ch in channels.values() {
        if !is_text_like(ch) {
            continue;
        }
        let mut targets: Vec<LockedTarget> = Vec::new();

        // @everyone: deny the lot.
        let existing = ch
            .permission_overwrites
            .iter()
            .find(|o| matches!(o.kind, PermissionOverwriteType::Role(r) if r == everyone));
        let (allow, deny) =
            existing.map(|o| (o.allow, o.deny)).unwrap_or((Permissions::empty(), Permissions::empty()));
        if let Some(plan) = plan_lock(allow, deny, perms) {
            let ok = ch
                .id
                .create_permission(
                    &ctx.http,
                    PermissionOverwrite {
                        allow: plan.allow,
                        deny: plan.deny,
                        kind: PermissionOverwriteType::Role(everyone),
                    },
                )
                .await
                .is_ok();
            if ok {
                targets.push(LockedTarget {
                    id: everyone.to_string(),
                    kind: "role".to_string(),
                    denied: plan.newly_denied.bits().to_string(),
                    allow_removed: plan.allow_removed.bits().to_string(),
                });
            }
        }

        // Everyone else: drop explicit allows that would out-rank that deny.
        for ow in &ch.permission_overwrites {
            let (id, kind) = match ow.kind {
                PermissionOverwriteType::Role(r) if r == everyone => continue,
                PermissionOverwriteType::Role(r) => (r.get(), "role"),
                PermissionOverwriteType::Member(u) => (u.get(), "member"),
                _ => continue,
            };
            let granted = perms & ow.allow;
            if granted.is_empty() {
                continue;
            }
            let mut allow = ow.allow;
            allow.remove(granted);
            let ok = ch
                .id
                .create_permission(
                    &ctx.http,
                    PermissionOverwrite { allow, deny: ow.deny, kind: ow.kind },
                )
                .await
                .is_ok();
            if ok {
                targets.push(LockedTarget {
                    id: id.to_string(),
                    kind: kind.to_string(),
                    denied: "0".to_string(),
                    allow_removed: granted.bits().to_string(),
                });
            }
        }

        if !targets.is_empty() {
            outcome.locked += 1;
            outcome.changes.push(LockedChannel { channel_id: ch.id.to_string(), targets });
        }
    }
    outcome
}

/// Undo exactly what a lockdown changed, and nothing else.
///
/// Without a record (a lock written before this was tracked) it falls back to
/// clearing the talking permissions everywhere, which is what the old code
/// always did. That can reopen a channel that was read-only beforehand, so it
/// is the fallback rather than the rule.
pub async fn unlock_all_text_channels(ctx: &Context, guild_id: GuildId) -> usize {
    use serenity::model::channel::{PermissionOverwrite, PermissionOverwriteType};

    let record = lockdown::changed_channels(&guild_id.to_string()).unwrap_or_default();
    let channels = guild_id.channels(&ctx.http).await.unwrap_or_default();
    let perms = lock_perms();

    if record.is_empty() {
        let everyone = RoleId::new(guild_id.get());
        let mut unlocked = 0;
        for ch in channels.values() {
            if is_text_like(ch) && set_send_messages(ctx, ch, everyone, None).await {
                unlocked += 1;
            }
        }
        return unlocked;
    }

    let mut unlocked = 0;
    for locked in &record {
        let Ok(raw) = locked.channel_id.parse::<u64>() else { continue };
        let Some(ch) = channels.values().find(|c| c.id.get() == raw) else { continue };
        let mut touched = false;

        for target in &locked.targets {
            let Ok(tid) = target.id.parse::<u64>() else { continue };
            let kind = match target.kind.as_str() {
                "member" => PermissionOverwriteType::Member(serenity::model::id::UserId::new(tid)),
                _ => PermissionOverwriteType::Role(RoleId::new(tid)),
            };
            let denied = Permissions::from_bits_truncate(target.denied.parse::<u64>().unwrap_or(0));
            let restored = Permissions::from_bits_truncate(target.allow_removed.parse::<u64>().unwrap_or(0));

            let current = ch.permission_overwrites.iter().find(|o| same_target(&o.kind, &kind));
            let (allow, deny) =
                current.map(|o| (o.allow, o.deny)).unwrap_or((Permissions::empty(), Permissions::empty()));
            let (allow, deny) = plan_unlock(allow, deny, denied, restored, perms);

            if ch
                .id
                .create_permission(&ctx.http, PermissionOverwrite { allow, deny, kind })
                .await
                .is_ok()
            {
                touched = true;
            }
        }
        if touched {
            unlocked += 1;
        }
    }
    unlocked
}

fn same_target(
    a: &serenity::model::channel::PermissionOverwriteType,
    b: &serenity::model::channel::PermissionOverwriteType,
) -> bool {
    use serenity::model::channel::PermissionOverwriteType as T;
    match (a, b) {
        (T::Role(x), T::Role(y)) => x == y,
        (T::Member(x), T::Member(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEND: Permissions = Permissions::SEND_MESSAGES;
    const IN_THREADS: Permissions = Permissions::SEND_MESSAGES_IN_THREADS;
    const VIEW: Permissions = Permissions::VIEW_CHANNEL;

    fn perms() -> Permissions {
        lock_perms()
    }

    /// The reported problem: lifting a lockdown reopened channels that were
    /// deliberately read-only beforehand. An already-locked channel must
    /// produce no plan, which keeps it out of the record entirely.
    #[test]
    fn an_already_locked_channel_is_not_touched_and_not_recorded() {
        // @everyone already denied Send Messages, e.g. an announcements channel.
        let plan = plan_lock(Permissions::empty(), perms(), perms());
        assert!(plan.is_none(), "a fully locked channel must not be re-locked or recorded");
    }

    /// A channel locked only part way still gets finished off, and only the
    /// bits that were actually missing are recorded.
    #[test]
    fn a_partly_locked_channel_records_only_the_bits_it_added() {
        // Send Messages already denied, but threads were left open.
        let plan = plan_lock(Permissions::empty(), SEND, perms()).expect("has work to do");
        assert!(!plan.newly_denied.contains(SEND), "Send Messages was already denied");
        assert!(plan.newly_denied.contains(IN_THREADS), "threads were still open");

        // Lifting restores only the thread bits, leaving the pre-existing
        // Send Messages deny in place.
        let (_, deny) = plan_unlock(plan.allow, plan.deny, plan.newly_denied, plan.allow_removed, perms());
        assert!(deny.contains(SEND), "the deny that predated the lockdown must survive the lift");
        assert!(!deny.contains(IN_THREADS), "what the lockdown added must come off");
    }

    #[test]
    fn an_open_channel_is_locked_then_restored_exactly() {
        let before_allow = VIEW;
        let before_deny = Permissions::empty();

        let plan = plan_lock(before_allow, before_deny, perms()).expect("open channel needs locking");
        assert!(plan.deny.contains(perms()), "everything that lets you talk must be denied");

        let (allow, deny) = plan_unlock(plan.allow, plan.deny, plan.newly_denied, plan.allow_removed, perms());
        assert_eq!(allow, before_allow, "allow must come back exactly as it was");
        assert_eq!(deny, before_deny, "deny must come back exactly as it was");
    }

    /// An explicit allow on a staff role out-ranks @everyone's deny, so the
    /// lock has to remove it or that role talks straight through the lockdown.
    #[test]
    fn an_explicit_allow_is_stripped_and_handed_back() {
        let before_allow = SEND | IN_THREADS | VIEW;
        let plan = plan_lock(before_allow, Permissions::empty(), perms()).expect("allow must be stripped");
        assert!(!plan.allow.contains(SEND), "the allow that beat the deny must go");
        assert!(plan.allow.contains(VIEW), "unrelated permissions are left alone");
        assert_eq!(plan.allow_removed, SEND | IN_THREADS);

        let (allow, _) = plan_unlock(plan.allow, plan.deny, plan.newly_denied, plan.allow_removed, perms());
        assert_eq!(allow, before_allow, "the allow must be handed back on lift");
    }

    /// Permissions the lockdown never touched are never restored either, so an
    /// admin editing a channel mid-lockdown does not get overwritten.
    #[test]
    fn the_lift_only_moves_bits_the_lockdown_owned() {
        // Lockdown denied the talking perms. Meanwhile an admin also denied
        // VIEW_CHANNEL by hand.
        let during_allow = Permissions::empty();
        let during_deny = perms() | VIEW;

        let (_, deny) = plan_unlock(during_allow, during_deny, perms(), Permissions::empty(), perms());
        assert!(deny.contains(VIEW), "an admin's own change must survive the lift");
        assert!(!deny.intersects(perms()), "the lockdown's own denies must come off");
    }

    /// Every way of talking is covered, threads included.
    #[test]
    fn lock_covers_threads_as_well_as_plain_messages() {
        let p = lock_perms();
        assert!(p.contains(Permissions::SEND_MESSAGES));
        assert!(p.contains(Permissions::SEND_MESSAGES_IN_THREADS));
        assert!(p.contains(Permissions::CREATE_PUBLIC_THREADS));
        assert!(p.contains(Permissions::CREATE_PRIVATE_THREADS));
    }
}
