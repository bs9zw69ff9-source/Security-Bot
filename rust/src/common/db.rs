//! Database (SQLite via rusqlite).
//!
//! Write-through persistence: fast in-memory maps stay the source of truth for
//! reads; every change is mirrored to a single ACID-safe .db file. Uses the
//! exact same `guardian.db` schema (one `(guild_id, data)` JSON-blob table per
//! feature) as the original bot, so an existing database carries straight over.

use once_cell::sync::Lazy;
use rusqlite::Connection;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

use super::config::root_file;

pub const TABLES: &[&str] = &[
    "guild_settings",
    "antiping",
    "warnings",
    "muted_roles",
    "snapshots",
    "failsafe",
    "mod_rates",
    "lockdown_state",
    "tickets",
    "ticket_channels",
    "applications",
    "chain_of_command",
];

static DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let path = std::env::var("GUARDIAN_DB_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| root_file("guardian.db"));
    let conn = Connection::open(&path).unwrap_or_else(|e| panic!("failed to open {}: {e}", path.display()));
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    for t in TABLES {
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {t} (guild_id TEXT PRIMARY KEY, data TEXT NOT NULL)"
        ))
        .unwrap_or_else(|e| panic!("failed to create table {t}: {e}"));
    }
    Mutex::new(conn)
});

/// Force the connection open (and run the CREATE TABLEs) at a known point.
pub fn init() {
    Lazy::force(&DB);
}

pub fn load_all<T: DeserializeOwned>(table: &str) -> HashMap<String, T> {
    let conn = match DB.lock() {
        Ok(c) => c,
        Err(e) => e.into_inner(),
    };
    let mut out = HashMap::new();
    let sql = format!("SELECT guild_id, data FROM {table}");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("⚠️ db load {table} failed: {e}");
            return out;
        }
    };
    let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)));
    if let Ok(rows) = rows {
        for row in rows.flatten() {
            // A row that no longer parses is skipped rather than fatal, matching
            // the JS `try { JSON.parse } catch {}` behaviour.
            if let Ok(v) = serde_json::from_str::<T>(&row.1) {
                out.insert(row.0, v);
            }
        }
    }
    out
}

/// Per-guild write (shard-safe: only ever touches this guild's row).
pub fn put<T: Serialize>(table: &str, guild_id: &str, value: &T) {
    let json = match serde_json::to_string(value) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("⚠️ db serialize {table}/{guild_id} failed: {e}");
            return;
        }
    };
    let conn = match DB.lock() {
        Ok(c) => c,
        Err(e) => e.into_inner(),
    };
    let sql = format!(
        "INSERT INTO {table} (guild_id, data) VALUES (?1, ?2) ON CONFLICT(guild_id) DO UPDATE SET data = excluded.data"
    );
    if let Err(e) = conn.execute(&sql, rusqlite::params![guild_id, json]) {
        eprintln!("⚠️ db write {table}/{guild_id} failed: {e}");
    }
}

pub fn delete(table: &str, guild_id: &str) {
    let conn = match DB.lock() {
        Ok(c) => c,
        Err(e) => e.into_inner(),
    };
    let sql = format!("DELETE FROM {table} WHERE guild_id = ?1");
    if let Err(e) = conn.execute(&sql, rusqlite::params![guild_id]) {
        eprintln!("⚠️ db delete {table}/{guild_id} failed: {e}");
    }
}

fn row_count(table: &str) -> i64 {
    let conn = match DB.lock() {
        Ok(c) => c,
        Err(e) => e.into_inner(),
    };
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap_or(0)
}

/// One-time import: if a legacy JSON file exists and the table is empty, load
/// it in. Same migration path the JS bot used, so a deployment that still has
/// the old JSON files sitting around picks them up identically.
pub fn import_json_if_present(table: &str, file: &std::path::Path) {
    if row_count(table) > 0 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(file) else { return };
    let Ok(map) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&text) else { return };

    let conn = match DB.lock() {
        Ok(c) => c,
        Err(e) => e.into_inner(),
    };
    let tx_sql = format!("INSERT INTO {table} (guild_id, data) VALUES (?1, ?2)");
    let mut ok = true;
    for (gid, val) in &map {
        let json = match serde_json::to_string(val) {
            Ok(j) => j,
            Err(_) => continue,
        };
        if conn.execute(&tx_sql, rusqlite::params![gid, json]).is_err() {
            ok = false;
        }
    }
    if ok {
        println!("📥 Imported {} → {table}", file.display());
    }
}

/// Local forensic trail - appended for every security event; survives a wiped
/// log channel.
pub fn append_forensic(guild_id: &str, kind: &str, data: serde_json::Value) {
    let mut entry = serde_json::Map::new();
    entry.insert("t".into(), serde_json::Value::String(iso_now()));
    entry.insert("guildId".into(), serde_json::Value::String(guild_id.to_string()));
    entry.insert("kind".into(), serde_json::Value::String(kind.to_string()));
    if let serde_json::Value::Object(extra) = data {
        for (k, v) in extra {
            entry.insert(k, v);
        }
    }
    let Ok(line) = serde_json::to_string(&serde_json::Value::Object(entry)) else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root_file("security_log.jsonl")) {
        let _ = writeln!(f, "{line}");
    }
}

/// RFC3339-ish UTC timestamp (`2026-08-06T09:30:00.000Z`), matching the
/// `new Date().toISOString()` format the JS forensic log wrote.
fn iso_now() -> String {
    let ms = super::config::now_ms().max(0);
    let (secs, millis) = (ms / 1000, ms % 1000);
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // Civil-from-days (Howard Hinnant's algorithm), epoch 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}
