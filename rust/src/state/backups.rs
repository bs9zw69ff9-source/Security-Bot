//! Server backups (Xenon-style): a copy of a server's settings, roles,
//! channels, emojis, bans and role assignments, owned by the user who made it
//! and loadable into any server that user owns.
//!
//! Backups are keyed by their own id rather than by guild. Only the metadata
//! stays in memory; the full backup is read from the database when it's loaded
//! or inspected, since role assignments for a big server run to megabytes.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use crate::common::config::now_ms;
use crate::common::db;

const TABLE: &str = "backups";
const INTERVAL_TABLE: &str = "backup_intervals";

/// Per-user cap on manual backups. Also keeps `/backup list` and the
/// autocomplete inside Discord's 25 entries.
pub const MAX_PER_USER: usize = 25;
pub const MESSAGES_PER_CHANNEL: usize = 250;
pub const INTERVAL_HOURS: (i64, i64) = (6, 168);

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BSettings {
    pub name: String,
    pub icon_url: Option<String>,
    pub verification_level: u8,
    pub default_notifications: u8,
    pub explicit_content_filter: u8,
    pub afk_channel: Option<String>,
    pub afk_timeout: u16,
    pub system_channel: Option<String>,
    pub everyone_permissions: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BRole {
    pub id: String,
    pub name: String,
    pub color: u32,
    pub hoist: bool,
    pub mentionable: bool,
    pub permissions: String,
    pub position: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BOverwrite {
    pub id: String,
    /// 0 = role, 1 = member.
    pub kind: u8,
    pub allow: String,
    pub deny: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BChannel {
    pub id: String,
    pub name: String,
    pub kind: u8,
    pub parent_id: Option<String>,
    pub position: i64,
    pub topic: Option<String>,
    pub nsfw: bool,
    pub rate_limit: u16,
    pub bitrate: Option<u32>,
    pub user_limit: Option<u32>,
    pub overwrites: Vec<BOverwrite>,
    /// Oldest first.
    #[serde(default)]
    pub messages: Vec<BMessage>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BMessage {
    pub author: String,
    pub avatar: String,
    pub content: String,
    pub embeds: Vec<serenity::model::channel::Embed>,
    pub attachments: Vec<BAttachment>,
    pub at: i64,
    pub pinned: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BAttachment {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BEmoji {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BBan {
    pub user_id: String,
    pub reason: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BMember {
    pub user_id: String,
    pub nick: Option<String>,
    /// Backup role ids.
    pub roles: Vec<String>,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub roles: usize,
    pub channels: usize,
    pub emojis: usize,
    pub bans: usize,
    pub members: usize,
    #[serde(default)]
    pub messages: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Backup {
    pub id: String,
    pub owner_id: String,
    pub guild_id: String,
    pub guild_name: String,
    pub created_at: i64,
    #[serde(default)]
    pub interval: bool,
    pub counts: Counts,
    pub settings: BSettings,
    pub roles: Vec<BRole>,
    pub channels: Vec<BChannel>,
    pub emojis: Vec<BEmoji>,
    pub bans: Vec<BBan>,
    pub members: Vec<BMember>,
}

/// What `/backup list` and the autocomplete need, without the payload.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub id: String,
    pub owner_id: String,
    pub guild_name: String,
    pub created_at: i64,
    #[serde(default)]
    pub interval: bool,
    pub counts: Counts,
}

impl From<&Backup> for Meta {
    fn from(b: &Backup) -> Self {
        Meta {
            id: b.id.clone(),
            owner_id: b.owner_id.clone(),
            guild_name: b.guild_name.clone(),
            created_at: b.created_at,
            interval: b.interval,
            counts: b.counts,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interval {
    /// Whoever turned it on. The backups are theirs, so the schedule stops if
    /// the server changes hands.
    pub owner_id: String,
    pub hours: i64,
    pub next_at: i64,
    /// The previous interval backup, replaced by the next one.
    pub last_backup: Option<String>,
}

static META: Lazy<Mutex<HashMap<String, Meta>>> = Lazy::new(|| Mutex::new(db::load_all(TABLE)));
static INTERVALS: Lazy<Mutex<HashMap<String, Interval>>> = Lazy::new(|| Mutex::new(db::load_all(INTERVAL_TABLE)));

fn meta() -> std::sync::MutexGuard<'static, HashMap<String, Meta>> {
    META.lock().unwrap_or_else(|e| e.into_inner())
}
fn intervals() -> std::sync::MutexGuard<'static, HashMap<String, Interval>> {
    INTERVALS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Short, unique, and easy to type. Ids aren't secret: every read checks the
/// owner.
pub fn new_id() -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) & 0xfff;
    format!("{}{seq:03x}", radix36(now_ms() as u64))
}

fn radix36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

pub fn save(b: &Backup) -> bool {
    if !db::put(TABLE, &b.id, b) {
        return false;
    }
    meta().insert(b.id.clone(), Meta::from(b));
    true
}

/// Whose backups a caller can reach: one user's, or everyone's (bot owners).
fn reachable(m: &Meta, owner: Option<&str>) -> bool {
    owner.is_none_or(|o| m.owner_id == o)
}

/// A backup, if it exists and `owner` can reach it.
pub fn get(id: &str, owner: Option<&str>) -> Option<Backup> {
    if !meta().get(id).is_some_and(|m| reachable(m, owner)) {
        return None;
    }
    db::get(TABLE, id)
}

/// Delete a backup `owner` can reach; false if there was nothing to delete.
pub fn delete(id: &str, owner: Option<&str>) -> bool {
    let mut map = meta();
    if !map.get(id).is_some_and(|m| reachable(m, owner)) {
        return false;
    }
    map.remove(id);
    drop(map);
    db::delete(TABLE, id);
    true
}

/// The backups `owner` can reach, newest first.
pub fn list(owner: Option<&str>) -> Vec<Meta> {
    let mut out: Vec<Meta> = meta().values().filter(|m| reachable(m, owner)).cloned().collect();
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

pub fn manual_count(owner_id: &str) -> usize {
    meta().values().filter(|m| m.owner_id == owner_id && !m.interval).count()
}

pub fn interval(guild_id: &str) -> Option<Interval> {
    intervals().get(guild_id).cloned()
}

pub fn set_interval(guild_id: &str, iv: Option<Interval>) -> bool {
    let mut map = intervals();
    match iv {
        Some(iv) => {
            map.insert(guild_id.to_string(), iv.clone());
            drop(map);
            db::put(INTERVAL_TABLE, guild_id, &iv)
        }
        None => {
            map.remove(guild_id);
            drop(map);
            db::delete(INTERVAL_TABLE, guild_id);
            true
        }
    }
}

/// Guilds whose interval backup is due.
pub fn due_intervals(now: i64) -> Vec<(String, Interval)> {
    intervals().iter().filter(|(_, iv)| iv.next_at <= now).map(|(g, iv)| (g.clone(), iv.clone())).collect()
}

/// Which channels and roles a load keeps, edits and deletes, worked out before
/// anything is touched. Matching is by name (and kind for channels), so loading
/// a backup over the same server edits things in place instead of recreating
/// them, and members keep their roles and channels keep their messages.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// backup role id -> live role id to edit.
    pub reuse_roles: HashMap<String, u64>,
    pub delete_roles: Vec<u64>,
    /// backup channel id -> live channel id to edit.
    pub reuse_channels: HashMap<String, u64>,
    pub delete_channels: Vec<u64>,
}

pub struct LiveRole {
    pub id: u64,
    pub name: String,
    /// Managed, @everyone, or above the bot: can't be edited or deleted.
    pub locked: bool,
}

pub struct LiveChannel {
    pub id: u64,
    pub name: String,
    pub kind: u8,
}

pub fn plan(
    backup: &Backup,
    live_roles: &[LiveRole],
    live_channels: &[LiveChannel],
    delete_roles: bool,
    delete_channels: bool,
    keep_channel: u64,
) -> Plan {
    let mut plan = Plan::default();

    let mut taken = std::collections::HashSet::new();
    for r in &backup.roles {
        if let Some(live) = live_roles.iter().find(|l| !l.locked && l.name == r.name && !taken.contains(&l.id)) {
            taken.insert(live.id);
            plan.reuse_roles.insert(r.id.clone(), live.id);
        }
    }
    if delete_roles {
        plan.delete_roles = live_roles.iter().filter(|l| !l.locked && !taken.contains(&l.id)).map(|l| l.id).collect();
    }

    let mut taken = std::collections::HashSet::new();
    for c in &backup.channels {
        if let Some(live) =
            live_channels.iter().find(|l| l.name == c.name && l.kind == c.kind && !taken.contains(&l.id))
        {
            taken.insert(live.id);
            plan.reuse_channels.insert(c.id.clone(), live.id);
        }
    }
    if delete_channels {
        // Children before categories, and never the channel the load is
        // reporting into.
        let mut doomed: Vec<&LiveChannel> =
            live_channels.iter().filter(|l| !taken.contains(&l.id) && l.id != keep_channel).collect();
        doomed.sort_by_key(|l| l.kind == 4);
        plan.delete_channels = doomed.into_iter().map(|l| l.id).collect();
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backup(owner: &str) -> Backup {
        Backup {
            id: new_id(),
            owner_id: owner.into(),
            guild_id: "1".into(),
            guild_name: "Test".into(),
            created_at: now_ms(),
            interval: false,
            counts: Counts::default(),
            settings: BSettings::default(),
            roles: vec![
                BRole { id: "10".into(), name: "Mod".into(), color: 0, hoist: false, mentionable: false, permissions: "0".into(), position: 2 },
                BRole { id: "11".into(), name: "Member".into(), color: 0, hoist: false, mentionable: false, permissions: "0".into(), position: 1 },
            ],
            channels: vec![BChannel {
                id: "20".into(),
                name: "general".into(),
                kind: 0,
                parent_id: None,
                position: 0,
                topic: None,
                nsfw: false,
                rate_limit: 0,
                bitrate: None,
                user_limit: None,
                overwrites: vec![],
                messages: vec![],
            }],
            emojis: vec![],
            bans: vec![],
            members: vec![],
        }
    }

    #[test]
    fn backups_belong_to_their_creator() {
        let b = backup("100");
        assert!(save(&b));
        assert!(get(&b.id, Some("100")).is_some());
        assert!(get(&b.id, Some("200")).is_none(), "someone else's backup must not load");
        assert!(!delete(&b.id, Some("200")));
        assert!(list(Some("200")).iter().all(|m| m.id != b.id));
        // Bot owners reach every backup.
        assert!(get(&b.id, None).is_some());
        assert!(list(None).iter().any(|m| m.id == b.id));
        assert!(delete(&b.id, Some("100")));
        assert!(get(&b.id, Some("100")).is_none());
        assert!(get(&b.id, None).is_none());
    }

    #[test]
    fn ids_are_unique() {
        let ids: std::collections::HashSet<String> = (0..500).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 500);
    }

    #[test]
    fn plan_reuses_by_name_and_deletes_the_rest() {
        let b = backup("1");
        let roles = [
            LiveRole { id: 1, name: "@everyone".into(), locked: true },
            LiveRole { id: 2, name: "Mod".into(), locked: false },
            LiveRole { id: 3, name: "Nuker".into(), locked: false },
            LiveRole { id: 4, name: "Bot".into(), locked: true },
        ];
        let chans = [
            LiveChannel { id: 50, name: "general".into(), kind: 0 },
            LiveChannel { id: 51, name: "general".into(), kind: 2 },
            LiveChannel { id: 52, name: "spam".into(), kind: 4 },
            LiveChannel { id: 53, name: "here".into(), kind: 0 },
        ];
        let p = plan(&b, &roles, &chans, true, true, 53);
        assert_eq!(p.reuse_roles.get("10"), Some(&2));
        assert!(!p.reuse_roles.contains_key("11"));
        assert_eq!(p.delete_roles, vec![3], "locked roles are never deleted");
        assert_eq!(p.reuse_channels.get("20"), Some(&50), "matched by name and kind");
        assert_eq!(p.delete_channels, vec![51, 52], "categories last, the reporting channel kept");

        let keep = plan(&b, &roles, &chans, false, false, 53);
        assert!(keep.delete_roles.is_empty() && keep.delete_channels.is_empty());
    }

    #[test]
    fn intervals_round_trip() {
        let iv = Interval { owner_id: "1".into(), hours: 24, next_at: 5, last_backup: None };
        assert!(set_interval("777", Some(iv)));
        assert!(due_intervals(10).iter().any(|(g, _)| g == "777"));
        assert!(!due_intervals(1).iter().any(|(g, _)| g == "777"));
        set_interval("777", None);
        assert!(interval("777").is_none());
    }
}
