//! Full-guild snapshot + rollback (survive & undo a nuke).

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serenity::builder::{CreateChannel, EditChannel, EditRole};
use serenity::client::Context;
use serenity::model::channel::{ChannelType, Message, PermissionOverwrite, PermissionOverwriteType, ReactionType};
use serenity::model::id::{ChannelId, GuildId, RoleId, UserId};
use serenity::model::Permissions;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{now_ms, root_file, CONFIG};
use crate::common::db;
use crate::common::embeds::{alert_owner, colors};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapRole {
    pub id: String,
    pub name: String,
    pub color: u32,
    pub hoist: bool,
    pub mentionable: bool,
    pub permissions: String,
    pub position: i64,
    pub members: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapOverwrite {
    pub id: String,
    /// 0 = role, 1 = member (Discord's own numbering, as the JS stored it).
    pub kind: u8,
    pub allow: String,
    pub deny: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapChannel {
    pub id: String,
    pub name: String,
    /// Discord's numeric channel type.
    pub kind: u8,
    pub parent_id: Option<String>,
    pub position: i64,
    pub topic: Option<String>,
    pub nsfw: bool,
    pub rate_limit: u16,
    pub overwrites: Vec<SnapOverwrite>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub taken_at: i64,
    pub name: String,
    pub roles: Vec<SnapRole>,
    pub channels: Vec<SnapChannel>,
}

static SNAPSHOTS: Lazy<Mutex<HashMap<String, Vec<Snapshot>>>> = Lazy::new(|| {
    db::import_json_if_present("snapshots", &root_file("guild_snapshot.json"));
    Mutex::new(db::load_all("snapshots"))
});

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Vec<Snapshot>>> {
    match SNAPSHOTS.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

pub fn snapshot_count(guild_id: &str) -> usize {
    lock().get(guild_id).map(|v| v.len()).unwrap_or(0)
}

pub fn list_snapshots(guild_id: &str) -> Vec<(i64, usize, usize)> {
    lock()
        .get(guild_id)
        .map(|v| v.iter().map(|s| (s.taken_at, s.roles.len(), s.channels.len())).collect())
        .unwrap_or_default()
}

fn channel_kind_num(kind: ChannelType) -> u8 {
    match kind {
        ChannelType::Text => 0,
        ChannelType::Private => 1,
        ChannelType::Voice => 2,
        ChannelType::GroupDm => 3,
        ChannelType::Category => 4,
        ChannelType::News => 5,
        ChannelType::Stage => 13,
        ChannelType::Forum => 15,
        _ => 0,
    }
}

fn kind_from_num(n: u8) -> ChannelType {
    match n {
        2 => ChannelType::Voice,
        4 => ChannelType::Category,
        5 => ChannelType::News,
        13 => ChannelType::Stage,
        15 => ChannelType::Forum,
        _ => ChannelType::Text,
    }
}

/// Capture roles (with membership) and channels (with overwrites).
pub async fn snapshot_guild(ctx: &Context, guild_id: GuildId) -> Option<(usize, usize)> {
    // A complete member list is required for accurate role membership; large
    // guilds don't get one from the gateway by default.
    let members = guild_id.members(&ctx.http, None, None).await.unwrap_or_default();
    let channels = guild_id.channels(&ctx.http).await.unwrap_or_default();
    let (guild_name, roles_map) = {
        let g = ctx.cache.guild(guild_id)?;
        (g.name.to_string(), g.roles.clone())
    };

    let mut roles: Vec<SnapRole> = roles_map
        .iter()
        .filter(|(id, r)| id.get() != guild_id.get() && !r.managed)
        .map(|(id, r)| SnapRole {
            id: id.to_string(),
            name: r.name.to_string(),
            color: r.colour.0,
            hoist: r.hoist,
            mentionable: r.mentionable,
            permissions: r.permissions.bits().to_string(),
            position: r.position as i64,
            members: members.iter().filter(|m| m.roles.contains(id)).map(|m| m.user.id.to_string()).collect(),
        })
        .collect();
    roles.sort_by_key(|r| r.position);

    let snap_channels: Vec<SnapChannel> = channels
        .iter()
        .map(|(id, c)| SnapChannel {
            id: id.to_string(),
            name: c.name.to_string(),
            kind: channel_kind_num(c.kind),
            parent_id: c.parent_id.map(|p| p.to_string()),
            position: c.position as i64,
            topic: c.topic.as_ref().map(|t| t.to_string()),
            nsfw: c.nsfw,
            rate_limit: c.rate_limit_per_user.unwrap_or(0),
            overwrites: c
                .permission_overwrites
                .iter()
                .map(|o| {
                    let (id, kind) = match o.kind {
                        PermissionOverwriteType::Role(r) => (r.to_string(), 0),
                        PermissionOverwriteType::Member(m) => (m.to_string(), 1),
                        _ => (String::new(), 0),
                    };
                    SnapOverwrite { id, kind, allow: o.allow.bits().to_string(), deny: o.deny.bits().to_string() }
                })
                .collect(),
        })
        .collect();

    let counts = (roles.len(), snap_channels.len());
    let snap = Snapshot { taken_at: now_ms(), name: guild_name, roles, channels: snap_channels };

    let mut map = lock();
    let arr = map.entry(guild_id.to_string()).or_default();
    arr.push(snap);
    while arr.len() > CONFIG.snapshot_max {
        arr.remove(0);
    }
    let snapshot_list = arr.clone();
    drop(map);
    db::put("snapshots", &guild_id.to_string(), &snapshot_list);
    Some(counts)
}

/// Restore the guild to look EXACTLY like the latest snapshot: deletes
/// anything not in it, corrects anything that drifted, re-syncs role
/// membership (adds AND removes), and recreates anything missing.
///
/// Destructive by design - requires a ✅ confirmation before touching anything.
pub async fn rollback_guild(ctx: &Context, guild_id: GuildId, msg: &Message) {
    let snap = lock().get(&guild_id.to_string()).and_then(|v| v.last().cloned());
    let Some(snap) = snap else {
        let _ = msg.reply(&ctx.http, "There is no snapshot saved yet. Take one with `!snapshot` first.").await;
        return;
    };

    let members = guild_id.members(&ctx.http, None, None).await.unwrap_or_default();
    let live_channels = guild_id.channels(&ctx.http).await.unwrap_or_default();
    let live_roles = match ctx.cache.guild(guild_id) {
        Some(g) => g.roles.clone(),
        None => return,
    };

    let snap_role_names: Vec<&String> = snap.roles.iter().map(|r| &r.name).collect();
    let extra_roles: Vec<RoleId> = live_roles
        .iter()
        .filter(|(id, r)| id.get() != guild_id.get() && !r.managed && !snap_role_names.contains(&&r.name.to_string()))
        .map(|(id, _)| *id)
        .collect();
    let snap_chan_keys: Vec<String> = snap.channels.iter().map(|c| format!("{}::{}", c.name, c.kind)).collect();
    let extra_channels: Vec<(ChannelId, ChannelType)> = live_channels
        .iter()
        .filter(|(_, c)| !snap_chan_keys.contains(&format!("{}::{}", c.name, channel_kind_num(c.kind))))
        .map(|(id, c)| (*id, c.kind))
        .collect();

    // Confirmation gate.
    let warning = msg
        .reply(
            &ctx.http,
            format!(
                "⚠️ **Full rollback to the snapshot from <t:{}:R>.** This will:\n• **Delete {}** role(s) not in that snapshot\n• **Delete {}** channel(s) not in that snapshot\n• Correct permissions/overwrites on everything else to match exactly\n• Re-sync role membership to match the snapshot (adds **and** removes members)\n\nAnything created since the snapshot was taken - legitimate or not - will be deleted. React with ✅ within 30s to confirm, or ignore to cancel.",
                snap.taken_at / 1000,
                extra_roles.len(),
                extra_channels.len()
            ),
        )
        .await;
    let Ok(warning) = warning else { return };
    let _ = warning.react(&ctx.http, ReactionType::Unicode("✅".to_string())).await;

    let author_id = msg.author.id;
    let confirmed = serenity::collector::ReactionCollector::new(&ctx.shard)
        .message_id(warning.id)
        .timeout(std::time::Duration::from_secs(30))
        .filter(move |r| {
            r.emoji.unicode_eq("✅") && r.user_id == Some(author_id)
        })
        .next()
        .await
        .is_some();
    if !confirmed {
        let _ = msg.reply(&ctx.http, "Rollback cancelled - I did not get a confirmation in time.").await;
        return;
    }

    let _ = msg
        .reply(
            &ctx.http,
            format!(
                "♻️ **Rolling back** to snapshot from <t:{}:R> - deleting extras, correcting drift, recreating missing…",
                snap.taken_at / 1000
            ),
        )
        .await;

    // 1) Delete anything not in the snapshot (non-categories first, tidier).
    let mut roles_deleted = 0usize;
    for rid in &extra_roles {
        if guild_id.delete_role(&ctx.http, *rid).await.is_ok() {
            roles_deleted += 1;
        }
    }
    let mut chans_deleted = 0usize;
    let ordered: Vec<ChannelId> = extra_channels
        .iter()
        .filter(|(_, k)| *k != ChannelType::Category)
        .chain(extra_channels.iter().filter(|(_, k)| *k == ChannelType::Category))
        .map(|(id, _)| *id)
        .collect();
    for cid in ordered {
        if cid.delete(&ctx.http).await.is_ok() {
            chans_deleted += 1;
        }
    }

    // 2) Roles: correct existing (matched by name) or recreate missing.
    let mut role_map: HashMap<String, RoleId> = HashMap::new();
    let (mut roles_created, mut roles_corrected) = (0usize, 0usize);
    let mut by_position = snap.roles.clone();
    by_position.sort_by_key(|r| r.position);
    for sr in &by_position {
        let perms = Permissions::from_bits_truncate(sr.permissions.parse::<u64>().unwrap_or(0));
        let existing = ctx.cache.guild(guild_id).and_then(|g| {
            g.roles
                .iter()
                .find(|(id, r)| r.name == sr.name && !r.managed && id.get() != guild_id.get())
                .map(|(id, _)| *id)
        });
        let builder = EditRole::new()
            .name(sr.name.clone())
            .colour(sr.color as u64)
            .hoist(sr.hoist)
            .mentionable(sr.mentionable)
            .permissions(perms);
        match existing {
            Some(id) => {
                if guild_id.edit_role(&ctx.http, id, builder.audit_log_reason("Rollback: correct drifted role")).await.is_ok() {
                    roles_corrected += 1;
                }
                role_map.insert(sr.id.clone(), id);
            }
            None => {
                if let Ok(r) = guild_id.create_role(&ctx.http, builder.audit_log_reason("Rollback: recreate role")).await {
                    roles_created += 1;
                    role_map.insert(sr.id.clone(), r.id);
                }
            }
        }
    }
    for sr in &snap.roles {
        if let Some(id) = role_map.get(&sr.id) {
            let _ = guild_id
                .edit_role_position(&ctx.http, *id, sr.position.clamp(0, u16::MAX as i64) as u16)
                .await;
        }
    }

    // 2b) Re-sync role membership exactly: add whoever's missing, remove
    //     whoever holds it now but isn't in the snapshot's member list.
    let (mut members_added, mut members_removed) = (0usize, 0usize);
    for sr in &snap.roles {
        let Some(live) = role_map.get(&sr.id) else { continue };
        for uid in &sr.members {
            let Ok(raw) = uid.parse::<u64>() else { continue };
            let user_id = UserId::new(raw);
            if members.iter().any(|m| m.user.id == user_id && m.roles.contains(live)) {
                continue;
            }
            let Ok(m) = guild_id.member(&ctx.http, user_id).await else { continue };
            if m.add_role(&ctx.http, *live).await.is_ok() {
                members_added += 1;
            }
        }
        for m in members.iter().filter(|m| m.roles.contains(live)) {
            if sr.members.contains(&m.user.id.to_string()) {
                continue;
            }
            if m.remove_role(&ctx.http, *live).await.is_ok() {
                members_removed += 1;
            }
        }
    }

    // Remap overwrite targets: @everyone is stable, roles remap by name,
    // members stay as-is.
    let remap = |ows: &[SnapOverwrite]| -> Vec<PermissionOverwrite> {
        let mut out = Vec::new();
        for o in ows {
            let allow = Permissions::from_bits_truncate(o.allow.parse::<u64>().unwrap_or(0));
            let deny = Permissions::from_bits_truncate(o.deny.parse::<u64>().unwrap_or(0));
            if o.kind == 0 {
                let target = if o.id == guild_id.to_string() {
                    o.id.parse::<u64>().ok().map(RoleId::new)
                } else {
                    role_map.get(&o.id).copied()
                };
                // References a role that no longer exists and wasn't recreated.
                let Some(rid) = target else { continue };
                out.push(PermissionOverwrite { allow, deny, kind: PermissionOverwriteType::Role(rid) });
            } else if let Ok(raw) = o.id.parse::<u64>() {
                out.push(PermissionOverwrite { allow, deny, kind: PermissionOverwriteType::Member(UserId::new(raw)) });
            }
        }
        out
    };

    // 3) Channels: categories first so children can attach to a fresh one.
    let mut chan_map: HashMap<String, ChannelId> = HashMap::new();
    let (mut chans_created, mut chans_corrected) = (0usize, 0usize);
    let cats: Vec<&SnapChannel> = snap.channels.iter().filter(|c| c.kind == 4).collect();
    let rest: Vec<&SnapChannel> = snap.channels.iter().filter(|c| c.kind != 4).collect();

    for c in cats {
        let overwrites = remap(&c.overwrites);
        let existing = ctx.cache.guild(guild_id).and_then(|g| {
            g.channels.iter().find(|(_, ch)| ch.kind == ChannelType::Category && ch.name == c.name).map(|(id, _)| *id)
        });
        match existing {
            Some(id) => {
                let _ = id.edit(&ctx.http, EditChannel::new().permissions(overwrites)).await;
                chans_corrected += 1;
                chan_map.insert(c.id.clone(), id);
            }
            None => {
                if let Ok(ch) = guild_id
                    .create_channel(
                        &ctx.http,
                        CreateChannel::new(c.name.clone())
                            .kind(ChannelType::Category)
                            .permissions(overwrites)
                            .audit_log_reason("Rollback"),
                    )
                    .await
                {
                    chans_created += 1;
                    chan_map.insert(c.id.clone(), ch.id);
                }
            }
        }
    }

    for c in rest {
        let overwrites = remap(&c.overwrites);
        let kind = kind_from_num(c.kind);
        let existing = ctx
            .cache
            .guild(guild_id)
            .and_then(|g| g.channels.iter().find(|(_, ch)| ch.name == c.name && ch.kind == kind).map(|(id, _)| *id));
        let parent = c.parent_id.as_ref().and_then(|p| chan_map.get(p).copied());
        match existing {
            Some(id) => {
                let mut edit = EditChannel::new().name(c.name.clone()).permissions(overwrites);
                if let Some(p) = parent {
                    edit = edit.category(Some(p));
                }
                if matches!(kind, ChannelType::Text | ChannelType::News) {
                    edit = edit.nsfw(c.nsfw).rate_limit_per_user(c.rate_limit);
                    if let Some(t) = &c.topic {
                        edit = edit.topic(t.clone());
                    }
                }
                let _ = id.edit(&ctx.http, edit).await;
                chans_corrected += 1;
                chan_map.insert(c.id.clone(), id);
            }
            None => {
                let mut builder =
                    CreateChannel::new(c.name.clone()).kind(kind).permissions(overwrites).audit_log_reason("Rollback");
                if let Some(p) = parent {
                    builder = builder.category(p);
                }
                if matches!(kind, ChannelType::Text | ChannelType::News) {
                    builder = builder.nsfw(c.nsfw).rate_limit_per_user(c.rate_limit);
                    if let Some(t) = &c.topic {
                        builder = builder.topic(t.clone());
                    }
                }
                if let Ok(ch) = guild_id.create_channel(&ctx.http, builder).await {
                    chans_created += 1;
                    chan_map.insert(c.id.clone(), ch.id);
                }
            }
        }
    }
    // Best-effort channel ordering.
    for c in &snap.channels {
        if let Some(id) = chan_map.get(&c.id) {
            let _ = id.edit(&ctx.http, EditChannel::new().position(c.position.clamp(0, u16::MAX as i64) as u16)).await;
        }
    }

    let report = format!(
        "♻️ **Full rollback complete.**\n• Roles: **{roles_created}** created, **{roles_corrected}** corrected, **{roles_deleted}** deleted (not in snapshot)\n• Channels: **{chans_created}** created, **{chans_corrected}** corrected, **{chans_deleted}** deleted (not in snapshot)\n• Role membership: **{members_added}** added, **{members_removed}** removed to match the snapshot\n_Recreated items get new Discord-assigned IDs; matched-by-name items were corrected in place._"
    );
    let _ = msg.reply(&ctx.http, &report).await;
    alert_owner(ctx, guild_id, &report, colors::SUCCESS, "ROLLBACK").await;
}
