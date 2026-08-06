//! Chain of command config (persisted to SQLite `chain_of_command`).
//!
//! A guild can have more than one board (e.g. "default" for staff, "police"
//! for the department), each posted to its own channel. Within a board, groups
//! are optional sub-headers (e.g. "Ranks" / "Sub Classes"); a group with no
//! label just renders as a flat list. Role ids are top-rank-first.

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::common::config::GUILD_ID;
use crate::common::db;

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChainGroup {
    /// `None` renders the roles as one flat list with no sub-header.
    pub label: Option<String>,
    pub role_ids: Vec<String>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Board {
    pub channel_id: String,
    pub message_id: String,
    pub title: String,
    pub groups: Vec<ChainGroup>,
}

/// Board key → board, insertion-ordered so `/chainofcommand list` is stable.
type Boards = IndexMap<String, Board>;

static CONFIGS: Lazy<Mutex<HashMap<String, Boards>>> = Lazy::new(|| Mutex::new(load_with_migration()));

/// Older configs stored one flat `{channelId, messageId, roleIds}` per guild
/// (no board key, no groups). Wrap that into a "default" board so an
/// already-posted board keeps editing the same message instead of duplicating.
fn load_with_migration() -> HashMap<String, Boards> {
    let raw: HashMap<String, serde_json::Value> = db::load_all("chain_of_command");
    let mut out: HashMap<String, Boards> = HashMap::new();

    for (gid, value) in raw {
        let Some(obj) = value.as_object() else { continue };
        let is_legacy = !obj.contains_key("default") && (obj.contains_key("channelId") || obj.contains_key("roleIds"));

        if is_legacy {
            let role_ids: Vec<String> = obj
                .get("roleIds")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let board = Board {
                channel_id: obj.get("channelId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                message_id: obj.get("messageId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                title: String::new(),
                groups: vec![ChainGroup { label: None, role_ids }],
            };
            let mut boards: Boards = IndexMap::new();
            boards.insert("default".to_string(), board);
            db::put("chain_of_command", &gid, &boards);
            out.insert(gid, boards);
        } else if let Ok(boards) = serde_json::from_value::<Boards>(value) {
            out.insert(gid, boards);
        }
    }
    out
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Boards>> {
    match CONFIGS.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

pub fn get_chain_keys(guild_id: &str) -> Vec<String> {
    lock().get(guild_id).map(|b| b.keys().cloned().collect()).unwrap_or_default()
}

pub fn get_chain(guild_id: &str, key: &str) -> Board {
    lock().get(guild_id).and_then(|b| b.get(key).cloned()).unwrap_or_default()
}

pub fn update_chain<F: FnOnce(&mut Board)>(guild_id: &str, key: &str, f: F) {
    let mut map = lock();
    let boards = map.entry(guild_id.to_string()).or_default();
    let board = boards.entry(key.to_string()).or_default();
    f(board);
    let snapshot = boards.clone();
    drop(map);
    db::put("chain_of_command", guild_id, &snapshot);
}

/// All role ids tracked by any board in a guild - used to decide whether a
/// role change is worth reacting to at all.
pub fn get_all_chain_role_ids(guild_id: &str) -> HashSet<String> {
    lock()
        .get(guild_id)
        .map(|boards| {
            boards
                .values()
                .flat_map(|b| b.groups.iter().flat_map(|g| g.role_ids.iter().cloned()))
                .collect()
        })
        .unwrap_or_default()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// One-time seed: the requested chain-of-command role hierarchy for the HOME
/// guild (GUILD_ID) only, top rank first, as the "default" board. Never
/// overwrites an existing configuration.
pub fn migrate_chain_of_command_to_home_guild() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    if !get_chain(home, "default").groups.is_empty() {
        return;
    }
    update_chain(home, "default", |b| {
        b.groups = vec![ChainGroup {
            label: None,
            role_ids: strings(&[
                "1528754338472792085",
                "1528754340964208702",
                "1529251949671743671",
                "1529251385424609350",
                "1529252146925535345",
                "1529184247800266834",
                "1529184185137500192",
                "1529252370586796213",
                "1529184126358257684",
            ]),
        }];
    });
    println!("📋 Seeded chain-of-command role order for home guild ({home})");
}

/// One-time seed: the police chain-of-command board for the HOME guild only -
/// its own channel, and two labeled groups (Ranks, then Sub Classes).
pub fn migrate_police_chain_of_command_to_home_guild() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    if !get_chain(home, "police").groups.is_empty() {
        return;
    }
    update_chain(home, "police", |b| {
        b.channel_id = "1529246137087951100".to_string();
        b.title = "🚓 Police Chain of Command".to_string();
        b.groups = vec![
            ChainGroup {
                label: Some("Ranks".to_string()),
                role_ids: strings(&[
                    "1528754354264342633",
                    "1528754356063703100",
                    "1528754356688781375",
                    "1528754359947624639",
                    "1528754360845078720",
                    "1528754361906499584",
                    "1528754362921254942",
                    "1528754363726827572",
                ]),
            },
            ChainGroup {
                label: Some("Sub Classes".to_string()),
                role_ids: strings(&["1528754365739958292", "1528754366851317872", "1528754367963074591"]),
            },
        ];
    });
    println!("📋 Seeded police chain-of-command board for home guild ({home})");
}
