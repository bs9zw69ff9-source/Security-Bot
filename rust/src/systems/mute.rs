//! Mute / unmute utilities (strips + stashes roles, restores on unmute) and
//! the boot recovery that re-applies, reschedules, or expires them.

use serenity::client::Context;
use serenity::model::id::{GuildId, RoleId, UserId};
use std::time::Duration;

use crate::common::config::now_ms;
use crate::common::embeds::{colors, sec_log};
use crate::common::guildinfo::GuildInfo;
use crate::state::guild_settings::gc;
use crate::state::lockdown;
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

fn is_text_like(ch: &serenity::model::channel::GuildChannel) -> bool {
    use serenity::model::channel::ChannelType;
    matches!(ch.kind, ChannelType::Text | ChannelType::News)
}

/// Reopen every text channel and clear lockdown state for a guild (shared by
/// the raid auto-lift timer, /panic unlock, and boot recovery of an expired
/// lock).
pub async fn lift_lockdown_channels(ctx: &Context, guild_id: GuildId, note: &str) {
    let channels = guild_id.channels(&ctx.http).await.unwrap_or_default();
    let everyone = RoleId::new(guild_id.get());
    for ch in channels.values() {
        if is_text_like(ch) {
            set_send_messages(ctx, ch, everyone, None).await;
        }
    }
    lockdown::clear_lockdown(&guild_id.to_string());
    sec_log(ctx, guild_id, "Lockdown Lifted", note, colors::SUCCESS).await;
}

/// Boot recovery: reschedule / expire raid lockdowns; leave panic and
/// nuke-storm lockdowns active (they have no auto-expiry, same as before a
/// restart).
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

/// Lock every text channel in a guild for @everyone. Shared by the raid
/// responder, the nuke-storm emergency lock, and /panic.
pub async fn lock_all_text_channels(ctx: &Context, guild_id: GuildId) -> usize {
    let channels = guild_id.channels(&ctx.http).await.unwrap_or_default();
    let everyone = RoleId::new(guild_id.get());
    let mut locked = 0;
    for ch in channels.values() {
        if is_text_like(ch) && set_send_messages(ctx, ch, everyone, Some(false)).await {
            locked += 1;
        }
    }
    locked
}

/// Unlock every text channel without touching stored lockdown state - used by
/// the /panic toggle, which reports its own count.
pub async fn unlock_all_text_channels(ctx: &Context, guild_id: GuildId) -> usize {
    let channels = guild_id.channels(&ctx.http).await.unwrap_or_default();
    let everyone = RoleId::new(guild_id.get());
    let mut unlocked = 0;
    for ch in channels.values() {
        if is_text_like(ch) && set_send_messages(ctx, ch, everyone, None).await {
            unlocked += 1;
        }
    }
    unlocked
}
