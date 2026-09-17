//! Hidden owner-only FAILSAFE (message commands, NOT slash-registered).
//!
//! Target roles are configured per guild via `/setup failsafe`, not hardcoded,
//! so this works for whatever server the bot is running in.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serenity::builder::EditRole;
use serenity::client::Context;
use serenity::model::channel::{Message, PermissionOverwrite, PermissionOverwriteType};
use serenity::model::id::{ChannelId, RoleId, UserId};
use serenity::model::Permissions;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{now_ms, root_file};
use crate::common::db;
use crate::common::embeds::{alert_owner, colors};
use crate::common::guildinfo::GuildInfo;
use crate::state::guild_settings::gc;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedOverwrite {
    pub channel_id: String,
    /// Bitfields stored as strings, exactly as the JS version wrote them.
    pub allow: String,
    pub deny: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedRole {
    pub original_id: String,
    pub name: String,
    pub color: u32,
    pub hoist: bool,
    pub mentionable: bool,
    pub permissions: String,
    pub position: i64,
    pub members: Vec<String>,
    #[serde(default)]
    pub overwrites: Vec<SavedOverwrite>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailsafeBackup {
    pub saved_at: i64,
    pub roles: Vec<SavedRole>,
}

static BACKUP: Lazy<Mutex<HashMap<String, FailsafeBackup>>> = Lazy::new(|| {
    db::import_json_if_present("failsafe", &root_file("failsafe_backup.json"));
    Mutex::new(db::load_all("failsafe"))
});

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, FailsafeBackup>> {
    match BACKUP.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// `!failsafe` - back up the target roles, delete them, and kick every bot.
pub async fn run_failsafe(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    let failsafe_role_ids = gc(&guild_id.to_string()).failsafe_role_ids;
    if failsafe_role_ids.is_empty() {
        let _ = msg
            .reply(&ctx.http, "You haven't picked any failsafe roles for this server yet. Add some with `/setup failsafe action:add role:@Role` first.")
            .await;
        return;
    }

    let _ = msg.reply(&ctx.http, "🛡️ **FAILSAFE engaged** - backing up, then purging roles & bots…").await;
    // Full cache for accurate membership + bot list.
    let members = guild_id.members(&ctx.http, None, None).await.unwrap_or_default();
    let channels = guild_id.channels(&ctx.http).await.unwrap_or_default();
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else { return };

    // 1) Snapshot target roles BEFORE deletion (so !restore can rebuild them).
    let mut snapshot: Vec<SavedRole> = Vec::new();
    for id in &failsafe_role_ids {
        let Ok(raw) = id.parse::<u64>() else { continue };
        let role_id = RoleId::new(raw);
        let Some(r) = info.roles.get(&role_id) else { continue };

        // Capture this role's permission overwrite on every channel.
        let mut overwrites = Vec::new();
        for (cid, ch) in &channels {
            for ow in &ch.permission_overwrites {
                if !matches!(ow.kind, PermissionOverwriteType::Role(rid) if rid == role_id) {
                    continue;
                }
                if ow.allow.is_empty() && ow.deny.is_empty() {
                    continue;
                }
                overwrites.push(SavedOverwrite {
                    channel_id: cid.to_string(),
                    allow: ow.allow.bits().to_string(),
                    deny: ow.deny.bits().to_string(),
                });
            }
        }
        snapshot.push(SavedRole {
            original_id: role_id.to_string(),
            name: r.name.clone(),
            color: r.colour,
            hoist: r.hoist,
            mentionable: r.mentionable,
            permissions: r.permissions.bits().to_string(),
            position: r.position,
            members: members.iter().filter(|m| m.roles.contains(&role_id)).map(|m| m.user.id.to_string()).collect(),
            overwrites,
        });
    }
    let backup = FailsafeBackup { saved_at: now_ms(), roles: snapshot.clone() };
    lock().insert(guild_id.to_string(), backup.clone());
    db::put("failsafe", &guild_id.to_string(), &backup);

    // 2) Delete the target roles.
    let mut deleted = 0usize;
    let mut failed_roles: Vec<String> = Vec::new();
    for id in &failsafe_role_ids {
        let Ok(raw) = id.parse::<u64>() else { continue };
        let role_id = RoleId::new(raw);
        let Some(r) = info.roles.get(&role_id) else { continue };
        if !info.role_editable(role_id) {
            failed_roles.push(format!("{} (above me)", r.name));
            continue;
        }
        if guild_id.delete_role(&ctx.http, role_id).await.is_ok() {
            deleted += 1;
        } else {
            failed_roles.push(r.name.clone());
        }
    }

    // 3) Kick every bot (except myself).
    let me = ctx.cache.current_user().id;
    let mut kicked = 0usize;
    let mut failed_bots: Vec<String> = Vec::new();
    for m in members.iter().filter(|m| m.user.bot && m.user.id != me) {
        if guild_id.kick_with_reason(&ctx.http, m.user.id, "Failsafe: owner purge").await.is_ok() {
            kicked += 1;
        } else {
            failed_bots.push(m.user.tag());
        }
    }

    let report = format!(
        "🛡️ **Failsafe complete.**\n• Roles backed up: **{}**\n• Roles deleted: **{deleted}**{}\n• Bots kicked: **{kicked}**{}\nRun `!restore` to rebuild the roles.",
        snapshot.len(),
        if failed_roles.is_empty() { String::new() } else { format!(" - failed: {}", failed_roles.join(", ")) },
        if failed_bots.is_empty() { String::new() } else { format!(" - failed: {}", failed_bots.join(", ")) },
    );
    let _ = msg.reply(&ctx.http, &report).await;
    alert_owner(ctx, guild_id, &report, colors::NUKE, "FAILSAFE").await;
}

/// `!restore` - recreate the backed-up roles in the same position, with their
/// channel access and members.
pub async fn run_restore(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    let backup = lock().get(&guild_id.to_string()).cloned();
    let Some(backup) = backup.filter(|b| !b.roles.is_empty()) else {
        let _ = msg.reply(&ctx.http, "I haven't got a failsafe backup for this server.").await;
        return;
    };

    let _ = msg.reply(&ctx.http, format!("♻️ **Restoring {} role(s)…**", backup.roles.len())).await;

    // Recreate roles (highest original position first keeps creation order sane).
    let mut ordered = backup.roles.clone();
    ordered.sort_by(|a, b| b.position.cmp(&a.position));
    let mut created: Vec<(SavedRole, RoleId)> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for saved in ordered {
        let perms = Permissions::from_bits_truncate(saved.permissions.parse::<u64>().unwrap_or(0));
        let builder = EditRole::new()
            .name(saved.name.clone())
            .colour(saved.color as u64)
            .hoist(saved.hoist)
            .mentionable(saved.mentionable)
            .permissions(perms)
            .audit_log_reason("Failsafe restore");
        match guild_id.create_role(&ctx.http, builder).await {
            Ok(r) => created.push((saved, r.id)),
            Err(_) => failed.push(saved.name.clone()),
        }
    }

    // Restore exact positions (best-effort under my own top role).
    for (saved, role_id) in &created {
        let _ = guild_id
            .edit_role_position(&ctx.http, *role_id, saved.position.clamp(0, u16::MAX as i64) as u16)
            .await;
    }

    // Restore each role's channel access → rebuilds visible channels.
    let mut ow_restored = 0usize;
    for (saved, role_id) in &created {
        for ow in &saved.overwrites {
            let Ok(cid) = ow.channel_id.parse::<u64>() else { continue };
            let allow = Permissions::from_bits_truncate(ow.allow.parse::<u64>().unwrap_or(0));
            let deny = Permissions::from_bits_truncate(ow.deny.parse::<u64>().unwrap_or(0));
            if ChannelId::new(cid)
                .create_permission(
                    &ctx.http,
                    PermissionOverwrite { allow, deny, kind: PermissionOverwriteType::Role(*role_id) },
                )
                .await
                .is_ok()
            {
                ow_restored += 1;
            }
        }
    }

    // Re-assign the roles to the members who had them.
    let mut reassigned = 0usize;
    for (saved, role_id) in &created {
        for uid in &saved.members {
            let Ok(raw) = uid.parse::<u64>() else { continue };
            let Ok(m) = guild_id.member(&ctx.http, UserId::new(raw)).await else { continue };
            if m.add_role(&ctx.http, *role_id).await.is_ok() {
                reassigned += 1;
            }
        }
    }

    let report = format!(
        "♻️ **Restore complete.**\n• Roles recreated: **{}/{}**{}\n• Channel overwrites restored: **{ow_restored}**\n• Member assignments restored: **{reassigned}**\n_Note: recreated roles get new IDs (Discord assigns them) - names, colors, permissions, positions, channel access, and members are preserved._",
        created.len(),
        backup.roles.len(),
        if failed.is_empty() { String::new() } else { format!(" - failed: {}", failed.join(", ")) },
    );
    let _ = msg.reply(&ctx.http, &report).await;
    alert_owner(ctx, guild_id, &report, colors::SUCCESS, "FAILSAFE RESTORE").await;
}
