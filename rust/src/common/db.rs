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

/// Where the database lives, so it can be named in an error rather than left
/// for the reader to guess.
pub fn db_path() -> std::path::PathBuf {
    std::env::var("GUARDIAN_DB_FILE").map(std::path::PathBuf::from).unwrap_or_else(|_| root_file("guardian.db"))
}

static DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let path = db_path();
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

/// Prove the database can actually be written to, by writing to it.
///
/// Opening a read-only SQLite file succeeds, and so does every read. Only the
/// first write fails, and it failed one row at a time in a warning nobody was
/// reading. Everything the bot seeds on boot then reappeared on the next boot,
/// because the flags saying it had already been seeded could not be stored
/// either, so the only thing that visibly went missing was configuration typed
/// in by hand. This turns that into one loud failure at startup.
pub fn check_writable() -> Result<(), String> {
    const PROBE: &str = "__writable_probe__";
    let conn = match DB.lock() {
        Ok(c) => c,
        Err(e) => e.into_inner(),
    };
    let write = conn.execute(
        "INSERT INTO guild_settings (guild_id, data) VALUES (?1, '{}') ON CONFLICT(guild_id) DO UPDATE SET data = excluded.data",
        rusqlite::params![PROBE],
    );
    match write {
        Ok(_) => {
            let _ = conn.execute("DELETE FROM guild_settings WHERE guild_id = ?1", rusqlite::params![PROBE]);
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Fold the write-ahead log back into the database file.
///
/// The connection lives in a `static`, and Rust does not drop statics at exit,
/// so it is never closed and SQLite never gets to checkpoint on its own. That
/// leaves an almost empty .db next to a WAL holding every actual change, which
/// recovers fine on the next open but is lost the moment anything copies,
/// moves or backs up the .db by itself.
pub fn checkpoint() {
    let conn = match DB.lock() {
        Ok(c) => c,
        Err(e) => e.into_inner(),
    };
    if let Err(e) = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE") {
        eprintln!("⚠️ couldn't fold the write-ahead log back into the database: {e}");
    }
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

/// How many writes have failed since the process started.
///
/// A failed write means the in-memory state and the file no longer agree, and
/// everything configured since then is going to vanish at the next restart. It
/// is worth being able to answer "has that happened" rather than hoping
/// somebody was reading the log at the time.
static WRITE_FAILURES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn write_failures() -> usize {
    WRITE_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Per-guild write (shard-safe: only ever touches this guild's row).
///
/// Returns whether it actually reached the file. Callers that are about to
/// tell somebody their configuration is saved should check it, because saying
/// "done" over a failed write is how a setting gets typed in twice.
pub fn put<T: Serialize>(table: &str, guild_id: &str, value: &T) -> bool {
    let json = match serde_json::to_string(value) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("⚠️ db serialize {table}/{guild_id} failed: {e}");
            WRITE_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return false;
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
        eprintln!("❌ couldn't save {table} for guild {guild_id} to {}: {e}", db_path().display());
        eprintln!("   That change is only in memory now, and will be gone at the next restart.");
        WRITE_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return false;
    }
    true
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

/// What is actually in the database, printed at startup.
///
/// This is the line that answers "did my configuration survive the restart?"
/// without anyone having to open SQLite. If a table that was configured before
/// the restart reads 0 here, nothing was saved; if it reads what it should and
/// the board is still missing from Discord, the problem is the posting rather
/// than the storage.
pub fn summary() -> String {
    let mut parts: Vec<String> = Vec::new();
    for t in TABLES {
        let n = row_count(t);
        if n > 0 {
            parts.push(format!("{t} {n}"));
        }
    }
    if parts.is_empty() {
        "empty".to_string()
    } else {
        parts.join(", ")
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
