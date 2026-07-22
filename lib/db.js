// ── Database (SQLite via better-sqlite3) ──────────────────────
// Write-through persistence: fast in-memory maps stay the source of truth for
// reads; every change is mirrored to a single ACID-safe .db file. This replaces
// the old JSON files (which corrupt under concurrent writes and don't scale).
// Requires: npm install better-sqlite3
const fs = require("fs");
const path = require("path");
const Database = require("better-sqlite3");

const DB_FILE = process.env.GUARDIAN_DB_FILE || path.join(__dirname, "..", "guardian.db");
const db = new Database(DB_FILE);
db.pragma("journal_mode = WAL");
db.pragma("busy_timeout = 5000");
for (const t of ["guild_settings", "antiping", "warnings", "muted_roles", "snapshots", "failsafe", "mod_rates", "lockdown_state", "tickets", "ticket_channels", "applications", "chain_of_command"])
  db.exec(`CREATE TABLE IF NOT EXISTS ${t} (guild_id TEXT PRIMARY KEY, data TEXT NOT NULL)`);

function dbLoadAll(table) {
  const out = {};
  for (const r of db.prepare(`SELECT guild_id, data FROM ${table}`).all()) {
    try { out[r.guild_id] = JSON.parse(r.data); } catch (_) {}
  }
  return out;
}
// Full-replace sync (used only for one-time bulk import into an empty table).
function dbReplaceAll(table, obj) {
  const tx = db.transaction(() => {
    db.prepare(`DELETE FROM ${table}`).run();
    const ins = db.prepare(`INSERT INTO ${table} (guild_id, data) VALUES (?, ?)`);
    for (const [gid, val] of Object.entries(obj)) if (val !== undefined) ins.run(gid, JSON.stringify(val));
  });
  tx();
}
// Per-guild write (shard-safe: only ever touches this guild's row). undefined ⇒ delete.
function dbPut(table, guildId, value) {
  if (value === undefined || value === null) {
    db.prepare(`DELETE FROM ${table} WHERE guild_id = ?`).run(guildId);
  } else {
    db.prepare(`INSERT INTO ${table} (guild_id, data) VALUES (?, ?) ON CONFLICT(guild_id) DO UPDATE SET data = excluded.data`)
      .run(guildId, JSON.stringify(value));
  }
}
// One-time import: if a legacy JSON file exists and the table is empty, load it in.
function importJsonIfPresent(table, file) {
  try {
    if (db.prepare(`SELECT COUNT(*) c FROM ${table}`).get().c > 0) return;
    if (!fs.existsSync(file)) return;
    const data = JSON.parse(fs.readFileSync(file, "utf8"));
    if (data && typeof data === "object") { dbReplaceAll(table, data); console.log(`📥 Imported ${file} → ${table}`); }
  } catch (e) { console.error(`⚠️ import ${file} failed:`, e.message); }
}

// Local forensic trail - appended for every security event; survives a wiped log channel.
const FORENSIC_FILE = path.join(__dirname, "..", "security_log.jsonl");
function appendForensic(guildId, kind, data) {
  try { fs.appendFileSync(FORENSIC_FILE, JSON.stringify({ t: new Date().toISOString(), guildId, kind, ...data }) + "\n"); }
  catch (_) {}
}

module.exports = { db, dbLoadAll, dbReplaceAll, dbPut, importJsonIfPresent, appendForensic, DB_FILE };
