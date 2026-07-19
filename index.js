// ============================================================
//  GUARDIAN BOT — Discord Security Bot (multi-server)
//  v3 — SQLite persistence, global commands, shard-ready
//  Required: npm install discord.js dotenv better-sqlite3
//  Optional (scale >2500 servers): run `node shard.js` instead of `node index.js`
// ============================================================

require("dotenv").config();
const fs   = require("fs");
const path = require("path");
const {
  Client, GatewayIntentBits, Partials, EmbedBuilder,
  PermissionsBitField, AuditLogEvent, Events,
  REST, Routes, SlashCommandBuilder, PermissionFlagsBits,
  ActivityType, ChannelType,
} = require("discord.js");

const TOKEN     = process.env.DISCORD_TOKEN;
const CLIENT_ID = process.env.CLIENT_ID;
const GUILD_ID  = process.env.GUILD_ID;

// ── HARDCODED OWNER ───────────────────────────────────────────
// Always fully trusted: immune to anti-nuke, rate limits, and all guards.
// Env override allowed, but defaults to the configured owner.
const BOT_OWNER_ID = process.env.BOT_OWNER_ID || "1014251293159731310";

const ANTIPING_FILE = path.join(__dirname, "antiping.json");
const MUTED_FILE    = path.join(__dirname, "mutedroles.json");
const WARN_FILE     = path.join(__dirname, "warnings.json");

// ── Database (SQLite via better-sqlite3) ──────────────────────
// Write-through persistence: fast in-memory maps stay the source of truth for
// reads; every change is mirrored to a single ACID-safe .db file. This replaces
// the old JSON files (which corrupt under concurrent writes and don't scale).
// Requires: npm install better-sqlite3
const Database = require("better-sqlite3");
const DB_FILE = path.join(__dirname, "guardian.db");
const db = new Database(DB_FILE);
db.pragma("journal_mode = WAL");
db.pragma("busy_timeout = 5000");
for (const t of ["guild_settings", "antiping", "warnings", "muted_roles", "snapshots", "failsafe"])
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

// ── Config ────────────────────────────────────────────────────
const config = {
  logChannelId:   process.env.LOG_CHANNEL_ID   || "",
  alertChannelId: process.env.ALERT_CHANNEL_ID || "", // criticals (owner gets pinged); falls back to log channel
  msgLogChannelId: process.env.MESSAGE_LOG_CHANNEL_ID || "", // deleted-message + image log
  modRoleId:      process.env.MOD_ROLE_ID      || "",
  muteRoleId:     process.env.MUTE_ROLE_ID     || "",

  nukeWhitelistRoleIds: (process.env.NUKE_WHITELIST_ROLE_IDS || "").split(",").filter(Boolean),
  nukeWhitelistUserIds: (process.env.NUKE_WHITELIST_USER_IDS || "").split(",").filter(Boolean),

  // Anti-spam
  spamThreshold:      parseInt(process.env.SPAM_THRESHOLD)      || 5,
  spamWindowMs:       parseInt(process.env.SPAM_WINDOW_MS)      || 3000,
  spamMuteMin:        parseInt(process.env.SPAM_MUTE_MIN)       || 10,
  spamMentionLimit:   parseInt(process.env.SPAM_MENTION_LIMIT)  || 6,    // mass-mention in a single message
  spamBlockInvites:   process.env.SPAM_BLOCK_INVITES !== "false",        // delete + mute on discord invite links
  spamDuplicateLimit: parseInt(process.env.SPAM_DUPLICATE_LIMIT) || 4,   // identical messages in window
  spamExemptStaff:    process.env.SPAM_EXEMPT_STAFF !== "false",         // mods/owner exempt; set false to stress-test yourself

  // Anti-raid
  raidJoinThreshold:   parseInt(process.env.RAID_JOIN_THRESHOLD)   || 10,
  raidWindowMs:        parseInt(process.env.RAID_WINDOW_MS)        || 10000,
  raidLockdownMin:     parseInt(process.env.RAID_LOCKDOWN_MIN)     || 5,
  raidKickNewOnLock:   process.env.RAID_KICK_NEW_ON_LOCK !== "false",     // kick brand-new accounts that join during lockdown
  raidMinAccountAgeMin:parseInt(process.env.RAID_MIN_ACCOUNT_AGE_MIN) || 1440, // <24h-old accounts are "new"

  // Anti-nuke (fast window)
  nukeWindowMs:             parseInt(process.env.NUKE_WINDOW_MS)             || 10000,
  nukeChannelThreshold:     parseInt(process.env.NUKE_CHANNEL_THRESHOLD)     || 3,
  nukeChannelCreateThresh:  parseInt(process.env.NUKE_CHANNEL_CREATE_THRESH) || 4,
  nukeRoleThreshold:        parseInt(process.env.NUKE_ROLE_THRESHOLD)        || 3,
  nukeRoleCreateThresh:     parseInt(process.env.NUKE_ROLE_CREATE_THRESH)    || 4,
  nukeBanThreshold:         parseInt(process.env.NUKE_BAN_THRESHOLD)         || 5,
  nukeKickThreshold:        parseInt(process.env.NUKE_KICK_THRESHOLD)        || 5,
  nukeWebhookThreshold:     parseInt(process.env.NUKE_WEBHOOK_THRESHOLD)     || 3,
  nukeMemberRoleThreshold:  parseInt(process.env.NUKE_MEMBER_ROLE_THRESH)    || 3, // dangerous-role grants
  nukeBotAddAction:         process.env.NUKE_BOT_ADD_ACTION || "kick",            // kick | alert
  nukeEmojiThreshold:       parseInt(process.env.NUKE_EMOJI_THRESHOLD)       || 5,

  // ── Nuke recovery + hardening ──
  snapshotIntervalMs: parseInt(process.env.SNAPSHOT_INTERVAL_MS) || 1800000, // auto full-guild snapshot every 30 min
  snapshotMax:        parseInt(process.env.SNAPSHOT_MAX)         || 5,        // rolling snapshots kept
  nukeStormThreshold: parseInt(process.env.NUKE_STORM_THRESHOLD) || 3,        // nuke responses within window → server-wide lockdown
  nukeStormWindowMs:  parseInt(process.env.NUKE_STORM_WINDOW_MS)  || 60000,
  scamBlock:          process.env.SCAM_BLOCK !== "false",                     // delete + mute on scam/phishing/grabber links
  ownerDM:            process.env.OWNER_DM   !== "false",                     // DM the owner on criticals (survives a nuked log channel)

  // ── Mod rate limits (24-hour rolling window, mods only — whitelisted users exempt) ──
  modBanLimit:      parseInt(process.env.MOD_BAN_LIMIT)      || 3,
  modKickLimit:     parseInt(process.env.MOD_KICK_LIMIT)     || 10,
  modMuteLimit:     parseInt(process.env.MOD_MUTE_LIMIT)     || 20,
  modPurgeLimit:    parseInt(process.env.MOD_PURGE_LIMIT)    || 5,
  modLockdownLimit: parseInt(process.env.MOD_LOCKDOWN_LIMIT) || 5,
  modWarnLimit:     parseInt(process.env.MOD_WARN_LIMIT)     || 30,
  modWindowMs:      parseInt(process.env.MOD_WINDOW_MS)      || 86400000, // 24h default

  // ── Warn escalation (0 = disabled) ──
  warnMuteAt:  parseInt(process.env.WARN_MUTE_AT)  || 3,
  warnKickAt:  parseInt(process.env.WARN_KICK_AT)  || 5,
  warnBanAt:   parseInt(process.env.WARN_BAN_AT)   || 7,
  warnMuteMin: parseInt(process.env.WARN_MUTE_MIN) || 60,

  // ── Anti-ping ──
  antiPingEnabled:       process.env.ANTIPING_ENABLED !== "false",
  antiPingAction:        process.env.ANTIPING_ACTION || "timeout", // none | warn | mute | timeout
  antiPingTimeoutMin:    parseInt(process.env.ANTIPING_TIMEOUT_MIN) || 5,
  antiPingDeleteMessage: process.env.ANTIPING_DELETE === "true",
  antiPingIgnoreReplies: process.env.ANTIPING_IGNORE_REPLIES !== "false",
  antiPingNotifyChannel: process.env.ANTIPING_NOTIFY !== "false",
  antiPingResponse:      process.env.ANTIPING_RESPONSE ||
                         "{user}, please don't ping {targets}. You have been {action}.",
  antiPingProtectedUserIds: (process.env.ANTIPING_PROTECTED_USER_IDS || "").split(",").filter(Boolean),
  antiPingProtectedRoleIds: (process.env.ANTIPING_PROTECTED_ROLE_IDS || "").split(",").filter(Boolean),
};

// Dangerous permissions used across anti-nuke checks.
const DANGER_PERMS = [
  PermissionsBitField.Flags.Administrator,
  PermissionsBitField.Flags.ManageGuild,
  PermissionsBitField.Flags.ManageChannels,
  PermissionsBitField.Flags.ManageRoles,
  PermissionsBitField.Flags.ManageWebhooks,
  PermissionsBitField.Flags.BanMembers,
  PermissionsBitField.Flags.KickMembers,
];

const INVITE_RE = /(discord\.(gg|io|me|li)|discordapp\.com\/invite|discord\.com\/invite)\/[a-z0-9-]+/i;

// High-precision scam/grabber patterns (typo-squats + known IP grabbers). Kept tight to avoid false positives.
const SCAM_RE = /(dlscord|disc0rd|discocrd|dlscordnitro|steamcommunilty|steancommunity|grabify\.link|iplogger\.(org|com|ru|co)|discordapp\.(ru|info)|free-?nitro-?gen|nitro-?free-?gift)/i;

// Local forensic trail — appended for every security event; survives a wiped log channel.
const FORENSIC_FILE = path.join(__dirname, "security_log.jsonl");
function appendForensic(guildId, kind, data) {
  try { fs.appendFileSync(FORENSIC_FILE, JSON.stringify({ t: new Date().toISOString(), guildId, kind, ...data }) + "\n"); }
  catch (_) {}
}

// ── State ─────────────────────────────────────────────────────
// Every tracker below is keyed (directly or via a "guildId:userId" composite
// key) by guild, so activity in one server can never trip detection or limits
// in another — required for correct multi-server operation.
const spamTracker = new Map();     // "gid:uid" -> [timestamps]
const dupeTracker = new Map();     // "gid:uid" -> { content, count, ts }
const joinTracker = new Map();     // gid -> [timestamps]
const nukeTracker = new Map();     // "gid:uid" -> dynamic action arrays
const modRateTracker = new Map();  // "gid:uid" -> 24h rolling action arrays
const lockdownGuilds = new Set();  // gid currently under lockdown
const isLockdown    = (gid) => lockdownGuilds.has(gid);
const setLockdown   = (gid) => lockdownGuilds.add(gid);
const clearLockdown = (gid) => lockdownGuilds.delete(gid);

// ── Anti-Ping runtime state (persisted to antiping.json) ──────
const antiPingDefaults = {
  enabled:          config.antiPingEnabled,
  action:           config.antiPingAction,
  timeoutMin:       config.antiPingTimeoutMin,
  deleteMessage:    config.antiPingDeleteMessage,
  ignoreReplies:    config.antiPingIgnoreReplies,
  notifyChannel:    config.antiPingNotifyChannel,
  responseTemplate: config.antiPingResponse,
  protectedUsers:   [...config.antiPingProtectedUserIds],
  protectedRoles:   [...config.antiPingProtectedRoleIds],
};
let antiPingStore = {}; // { [guildId]: { ...overrides } }
function loadAntiPing() { importJsonIfPresent("antiping", ANTIPING_FILE); antiPingStore = dbLoadAll("antiping"); }
function saveAntiPing(gid) { dbPut("antiping", gid, antiPingStore[gid]); }
loadAntiPing();
// Effective per-guild anti-ping config: stored override → .env default.
function ap(guild) {
  const id = typeof guild === "string" ? guild : guild?.id;
  return { ...antiPingDefaults, ...(id && antiPingStore[id] ? antiPingStore[id] : {}) };
}
function setAntiPing(guildId, patch) {
  antiPingStore[guildId] = { ...ap(guildId), ...patch };
  saveAntiPing(guildId);
}

// ── Muted-role stash state (persisted to mutedroles.json) ─────
// Shape: { [guildId]: { [userId]: { roles:[ids], reason, mutedAt, expiresAt|null } } }
let mutedRoles = {};
function loadMutedRoles() { importJsonIfPresent("muted_roles", MUTED_FILE); mutedRoles = dbLoadAll("muted_roles"); }
function saveMutedRoles(gid) { dbPut("muted_roles", gid, mutedRoles[gid]); }
loadMutedRoles();

// ── Warnings state (persisted to warnings.json) ───────────────
// Shape: { [guildId]: { [userId]: [{ reason, by, at }] } }
let warnings = {};
function loadWarnings() { importJsonIfPresent("warnings", WARN_FILE); warnings = dbLoadAll("warnings"); }
function saveWarnings(gid) { dbPut("warnings", gid, warnings[gid]); }
loadWarnings();

// ── Per-guild settings (set via /setup; override .env defaults) ──
const SETTINGS_FILE = path.join(__dirname, "guildsettings.json");
let guildSettings = {}; // { [guildId]: { modRoleId, muteRoleId, logChannelId, alertChannelId, msgLogChannelId, nukeWhitelistRoleIds[], nukeWhitelistUserIds[], failsafeRoleIds[] } }
function loadGuildSettings() { importJsonIfPresent("guild_settings", SETTINGS_FILE); guildSettings = dbLoadAll("guild_settings"); }
function saveGuildSettings(gid) { dbPut("guild_settings", gid, guildSettings[gid]); }
loadGuildSettings();

// Effective per-guild config — STRICTLY per server (no global fallback, so one
// guild's channels/roles/whitelist can never leak into another).
function gc(guild) {
  const id = typeof guild === "string" ? guild : guild?.id;
  const s = (id && guildSettings[id]) || {};
  return {
    modRoleId:            s.modRoleId       || "",
    muteRoleId:           s.muteRoleId      || "",
    logChannelId:         s.logChannelId    || "",
    alertChannelId:       s.alertChannelId  || "",
    msgLogChannelId:      s.msgLogChannelId || "",
    nukeWhitelistRoleIds: Array.isArray(s.nukeWhitelistRoleIds) ? s.nukeWhitelistRoleIds : [],
    nukeWhitelistUserIds: Array.isArray(s.nukeWhitelistUserIds) ? s.nukeWhitelistUserIds : [],
    failsafeRoleIds:      Array.isArray(s.failsafeRoleIds) ? s.failsafeRoleIds : [],
  };
}
function setGuild(guildId, key, value) {
  if (!guildSettings[guildId]) guildSettings[guildId] = {};
  guildSettings[guildId][key] = value;
  saveGuildSettings(guildId);
}

// One-time backward-compat: if legacy .env identity values are set, seed them into
// the HOME guild (GUILD_ID) ONLY — never applied globally, so other servers stay clean.
function migrateEnvToHomeGuild() {
  if (!GUILD_ID) return;
  const envDefaults = {
    modRoleId: config.modRoleId, muteRoleId: config.muteRoleId,
    logChannelId: config.logChannelId, alertChannelId: config.alertChannelId,
    msgLogChannelId: config.msgLogChannelId,
    nukeWhitelistRoleIds: config.nukeWhitelistRoleIds, nukeWhitelistUserIds: config.nukeWhitelistUserIds,
  };
  const cur = guildSettings[GUILD_ID] || {};
  let changed = false;
  for (const [k, v] of Object.entries(envDefaults)) {
    const empty = Array.isArray(v) ? v.length === 0 : !v;
    if (!empty && cur[k] === undefined) { cur[k] = v; changed = true; }
  }
  if (changed) { guildSettings[GUILD_ID] = cur; saveGuildSettings(GUILD_ID); console.log(`🔧 Seeded home guild (${GUILD_ID}) settings from .env`); }
}
migrateEnvToHomeGuild();

function addWarning(guildId, userId, reason, by) {
  if (!warnings[guildId]) warnings[guildId] = {};
  if (!warnings[guildId][userId]) warnings[guildId][userId] = [];
  warnings[guildId][userId].push({ reason, by, at: Date.now() });
  saveWarnings(guildId);
  return warnings[guildId][userId].length;
}
function getWarnings(guildId, userId) {
  return warnings[guildId]?.[userId] || [];
}
function clearWarnings(guildId, userId) {
  if (warnings[guildId]?.[userId]) { delete warnings[guildId][userId]; saveWarnings(guildId); }
}

// ── Hidden owner-only FAILSAFE (message commands, NOT slash-registered) ──
// Target roles are configured per guild via `/setup failsafe` (gc(guild).failsafeRoleIds) —
// NOT hardcoded, so this works for whatever server the bot is running in, not just one.
const FAILSAFE_FILE = path.join(__dirname, "failsafe_backup.json");

let failsafeBackup = {}; // { [guildId]: { savedAt, roles: [ {…role props, position, members[]} ] } }
function loadFailsafe() { importJsonIfPresent("failsafe", FAILSAFE_FILE); failsafeBackup = dbLoadAll("failsafe"); }
function saveFailsafe(gid) { dbPut("failsafe", gid, failsafeBackup[gid]); }
loadFailsafe();

// !failsafe — back up the target roles, delete them, and kick every bot.
async function runFailsafe(message) {
  const guild = message.guild;
  const failsafeRoleIds = gc(guild).failsafeRoleIds;
  if (!failsafeRoleIds.length)
    return message.reply("❌ No failsafe roles configured for this server. Run `/setup failsafe action:add role:@Role` first.").catch(() => {});

  await message.reply("🛡️ **FAILSAFE engaged** — backing up, then purging roles & bots…").catch(() => {});
  await guild.members.fetch().catch(() => {}); // full cache for accurate membership + bot list

  // 1) Snapshot target roles BEFORE deletion (so /restore can rebuild them).
  const snapshot = [];
  for (const id of failsafeRoleIds) {
    const role = guild.roles.cache.get(id);
    if (!role) continue;
    // Capture this role's permission overwrite on every channel (its visibility/access).
    const overwrites = [];
    for (const ch of guild.channels.cache.values()) {
      const ow = ch.permissionOverwrites?.cache?.get(role.id);
      if (!ow) continue;
      const allow = ow.allow.bitfield.toString();
      const deny  = ow.deny.bitfield.toString();
      if (allow === "0" && deny === "0") continue;
      overwrites.push({ channelId: ch.id, allow, deny });
    }
    snapshot.push({
      originalId:  role.id,
      name:        role.name,
      color:       role.color,
      hoist:       role.hoist,
      mentionable: role.mentionable,
      permissions: role.permissions.bitfield.toString(),
      position:    role.position,
      members:     role.members.map(m => m.id),
      overwrites,
    });
  }
  failsafeBackup[guild.id] = { savedAt: Date.now(), roles: snapshot };
  saveFailsafe(guild.id);

  // 2) Delete the target roles.
  let deleted = 0; const failedRoles = [];
  for (const id of failsafeRoleIds) {
    const role = guild.roles.cache.get(id);
    if (!role) continue;
    if (!role.editable) { failedRoles.push(`${role.name} (above me)`); continue; }
    const ok = await role.delete("Failsafe: owner purge").then(() => true).catch(() => false);
    if (ok) deleted++; else failedRoles.push(role.name);
  }

  // 3) Kick every bot (except myself).
  let kicked = 0; const failedBots = [];
  for (const m of guild.members.cache.filter(mm => mm.user.bot && mm.id !== client.user.id).values()) {
    if (!m.kickable) { failedBots.push(m.user.tag); continue; }
    const ok = await m.kick("Failsafe: owner purge").then(() => true).catch(() => false);
    if (ok) kicked++; else failedBots.push(m.user.tag);
  }

  const report =
    `🛡️ **Failsafe complete.**\n` +
    `• Roles backed up: **${snapshot.length}**\n` +
    `• Roles deleted: **${deleted}**` + (failedRoles.length ? ` — failed: ${failedRoles.join(", ")}` : "") + `\n` +
    `• Bots kicked: **${kicked}**` + (failedBots.length ? ` — failed: ${failedBots.join(", ")}` : "") + `\n` +
    `Run \`!restore\` to rebuild the roles.`;
  await message.reply(report).catch(() => {});
  alertOwner(guild, report, COLORS.nuke, "FAILSAFE");
}

// !restore — recreate the backed-up roles exactly, in the same position, with members.
async function runRestore(message) {
  const guild = message.guild;
  const backup = failsafeBackup[guild.id];
  if (!backup || !backup.roles?.length)
    return message.reply("❌ No failsafe backup found for this server.").catch(() => {});

  await message.reply(`♻️ **Restoring ${backup.roles.length} role(s)…**`).catch(() => {});

  // Recreate roles (highest original position first keeps creation order sane).
  const ordered = [...backup.roles].sort((a, b) => b.position - a.position);
  const created = []; const failed = [];
  for (const saved of ordered) {
    const role = await guild.roles.create({
      name:        saved.name,
      color:       saved.color,
      hoist:       saved.hoist,
      mentionable: saved.mentionable,
      permissions: BigInt(saved.permissions),
      reason:      "Failsafe restore",
    }).catch(() => null);
    if (!role) { failed.push(saved.name); continue; }
    created.push({ saved, role });
  }

  // Restore exact positions in one bulk call (best-effort under my own top role).
  if (created.length) {
    const positions = created.map(c => ({ role: c.role.id, position: c.saved.position }));
    await guild.roles.setPositions(positions).catch(() => {});
  }

  // Restore each role's channel access (permission overwrites) → rebuilds visible channels.
  let owRestored = 0;
  for (const { saved, role } of created) {
    for (const ow of saved.overwrites || []) {
      const ch = guild.channels.cache.get(ow.channelId);
      if (!ch || !ch.permissionOverwrites) continue;
      const opts = {};
      for (const p of new PermissionsBitField(BigInt(ow.allow)).toArray()) opts[p] = true;
      for (const p of new PermissionsBitField(BigInt(ow.deny)).toArray())  opts[p] = false;
      const ok = await ch.permissionOverwrites
        .edit(role.id, opts, { reason: "Failsafe restore: channel access" })
        .then(() => true).catch(() => false);
      if (ok) owRestored++;
    }
  }

  // Re-assign the roles to the members who had them.
  let reassigned = 0;
  for (const { saved, role } of created) {
    for (const uid of saved.members || []) {
      const m = await guild.members.fetch(uid).catch(() => null);
      if (!m) continue;
      const ok = await m.roles.add(role, "Failsafe restore: re-assign").then(() => true).catch(() => false);
      if (ok) reassigned++;
    }
  }

  const report =
    `♻️ **Restore complete.**\n` +
    `• Roles recreated: **${created.length}/${backup.roles.length}**` + (failed.length ? ` — failed: ${failed.join(", ")}` : "") + `\n` +
    `• Channel overwrites restored: **${owRestored}**\n` +
    `• Member assignments restored: **${reassigned}**\n` +
    `_Note: recreated roles get new IDs (Discord assigns them) — names, colors, permissions, positions, channel access, and members are preserved._`;
  await message.reply(report).catch(() => {});
  alertOwner(guild, report, COLORS.success, "FAILSAFE RESTORE");
}

// ── Full-guild snapshot + rollback (survive & undo a nuke) ────
const SNAPSHOT_FILE = path.join(__dirname, "guild_snapshot.json");
let snapshots = {}; // { [guildId]: [ { takenAt, name, roles[], channels[] } ] }  (newest last)
function loadSnapshots() { importJsonIfPresent("snapshots", SNAPSHOT_FILE); snapshots = dbLoadAll("snapshots"); }
function saveSnapshots(gid) { dbPut("snapshots", gid, snapshots[gid]); }
loadSnapshots();

function snapshotGuild(guild) {
  const roles = [...guild.roles.cache.values()]
    .filter(r => r.id !== guild.id && !r.managed)
    .sort((a, b) => a.position - b.position)
    .map(r => ({
      id: r.id, name: r.name, color: r.color, hoist: r.hoist, mentionable: r.mentionable,
      permissions: r.permissions.bitfield.toString(), position: r.position,
    }));
  const channels = [...guild.channels.cache.values()]
    .filter(c => !(c.isThread && c.isThread()))
    .map(c => ({
      id: c.id, name: c.name, type: c.type, parentId: c.parentId ?? null, position: c.rawPosition ?? 0,
      topic: c.topic ?? null, nsfw: c.nsfw ?? false, rateLimit: c.rateLimitPerUser ?? 0,
      bitrate: c.bitrate ?? null, userLimit: c.userLimit ?? null,
      overwrites: [...c.permissionOverwrites.cache.values()].map(o => ({
        id: o.id, type: o.type, allow: o.allow.bitfield.toString(), deny: o.deny.bitfield.toString(),
      })),
    }));
  const arr = snapshots[guild.id] || [];
  arr.push({ takenAt: Date.now(), name: guild.name, roles, channels });
  while (arr.length > config.snapshotMax) arr.shift();
  snapshots[guild.id] = arr;
  saveSnapshots(guild.id);
  return { roles: roles.length, channels: channels.length };
}

// Rebuild whatever the latest snapshot has that's currently missing (matched by name).
async function rollbackGuild(guild, message) {
  const snap = (snapshots[guild.id] || []).slice(-1)[0];
  if (!snap) { message?.reply("❌ No snapshot available yet. Run `!snapshot` first."); return; }
  message?.reply(`♻️ **Rolling back** to snapshot from <t:${Math.floor(snap.takenAt / 1000)}:R> — recreating missing roles & channels…`).catch(() => {});
  await guild.roles.fetch().catch(() => {});
  await guild.channels.fetch().catch(() => {});

  // Roles: map old-id → live role by name, recreating any that are gone.
  const roleMap = {}; let rolesCreated = 0;
  for (const sr of [...snap.roles].sort((a, b) => a.position - b.position)) {
    let live = guild.roles.cache.find(r => r.name === sr.name && !r.managed && r.id !== guild.id);
    if (!live) {
      live = await guild.roles.create({
        name: sr.name, color: sr.color, hoist: sr.hoist, mentionable: sr.mentionable,
        permissions: BigInt(sr.permissions), reason: "Rollback: recreate role",
      }).catch(() => null);
      if (live) rolesCreated++;
    }
    if (live) roleMap[sr.id] = live;
  }
  const rolePos = Object.entries(roleMap).map(([oldId, role]) => ({
    role: role.id, position: snap.roles.find(r => r.id === oldId)?.position || 1,
  }));
  if (rolePos.length) await guild.roles.setPositions(rolePos).catch(() => {});

  // Remap overwrite targets: @everyone (guild.id) is stable, roles remap by name, members stay.
  const remapOw = (ows) => {
    const out = [];
    for (const o of ows) {
      let id = o.id;
      if (o.type === 0) { // role overwrite
        if (id === guild.id) { /* @everyone — id stable */ }
        else if (roleMap[id]) id = roleMap[id].id;
        else continue; // references a role that no longer exists and wasn't recreated
      }
      out.push({ id, type: o.type, allow: BigInt(o.allow), deny: BigInt(o.deny) });
    }
    return out;
  };

  // Channels: categories first (so children can attach), then everything else. Match by name+type.
  const chanMap = {}; let chansCreated = 0;
  const cats = snap.channels.filter(c => c.type === ChannelType.GuildCategory);
  const rest = snap.channels.filter(c => c.type !== ChannelType.GuildCategory);
  for (const c of cats) {
    let live = guild.channels.cache.find(ch => ch.type === ChannelType.GuildCategory && ch.name === c.name);
    if (!live) {
      live = await guild.channels.create({ name: c.name, type: ChannelType.GuildCategory, permissionOverwrites: remapOw(c.overwrites), reason: "Rollback" }).catch(() => null);
      if (live) chansCreated++;
    }
    if (live) chanMap[c.id] = live;
  }
  for (const c of rest) {
    let live = guild.channels.cache.find(ch => ch.name === c.name && ch.type === c.type);
    if (!live) {
      const opts = { name: c.name, type: c.type, permissionOverwrites: remapOw(c.overwrites), reason: "Rollback" };
      if (c.parentId && chanMap[c.parentId]) opts.parent = chanMap[c.parentId].id;
      if (c.type === ChannelType.GuildText || c.type === ChannelType.GuildAnnouncement) {
        if (c.topic) opts.topic = c.topic;
        opts.nsfw = !!c.nsfw;
        if (c.rateLimit) opts.rateLimitPerUser = c.rateLimit;
      }
      if (c.type === ChannelType.GuildVoice || c.type === ChannelType.GuildStageVoice) {
        if (c.bitrate) opts.bitrate = c.bitrate;
        if (c.userLimit) opts.userLimit = c.userLimit;
      }
      live = await guild.channels.create(opts).catch(() => null);
      if (live) chansCreated++;
    }
    if (live) chanMap[c.id] = live;
  }
  // Best-effort channel ordering.
  for (const c of snap.channels) {
    const live = chanMap[c.id];
    if (live && typeof c.position === "number") live.setPosition(c.position).catch(() => {});
  }

  const report =
    `♻️ **Rollback complete.**\n` +
    `• Roles recreated: **${rolesCreated}**\n` +
    `• Channels recreated: **${chansCreated}**\n` +
    `_Existing items (matched by name) were left untouched; only missing ones were rebuilt. Recreated items get new IDs._`;
  message?.reply(report).catch(() => {});
  alertOwner(guild, report, COLORS.success, "ROLLBACK");
}

// ── Nuke-storm: multiple nuke responses in a short window → server-wide lockdown ──
const nukeStormTracker = new Map(); // gid -> [timestamps]
// Push a timestamp for this guild's nuke-storm tracker; returns true once the
// per-guild threshold is reached (and resets that guild's counter).
function bumpStorm(guildId) {
  const arr = (nukeStormTracker.get(guildId) || []).filter(t => Date.now() - t < config.nukeStormWindowMs);
  arr.push(Date.now());
  nukeStormTracker.set(guildId, arr);
  if (arr.length >= config.nukeStormThreshold) { nukeStormTracker.set(guildId, []); return true; }
  return false;
}
async function serverEmergencyLock(guild, reason) {
  alertOwner(guild,
    `🧨 **NUKE STORM DETECTED** — ${reason}.\nEngaging server-wide emergency lockdown: stripping dangerous roles from every non-whitelisted member and locking all channels.`,
    COLORS.nuke, "NUKE STORM LOCKDOWN");
  await guild.members.fetch().catch(() => {});
  for (const m of guild.members.cache.values()) {
    if (m.user.bot || isWhitelisted(m)) continue;
    const danger = m.roles.cache.filter(r => r.permissions.any(DANGER_PERMS) && r.editable);
    if (danger.size) m.roles.remove([...danger.keys()], "Nuke-storm lockdown").catch(() => {});
  }
  for (const ch of guild.channels.cache.values()) {
    if (ch.isTextBased() && !ch.isThread()) ch.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: false }).catch(() => {});
  }
  setLockdown(guild.id);
}

const client = new Client({
  intents: [
    GatewayIntentBits.Guilds,
    GatewayIntentBits.GuildMessages,
    GatewayIntentBits.GuildMembers,
    GatewayIntentBits.GuildBans,
    GatewayIntentBits.GuildWebhooks,
    GatewayIntentBits.MessageContent,
    GatewayIntentBits.GuildModeration,
  ],
  partials: [Partials.Message, Partials.Channel],
});

// ── Slash Commands ────────────────────────────────────────────
const commands = [
  new SlashCommandBuilder()
    .setName("mute").setDescription("Mute a member")
    .addUserOption(o => o.setName("user").setDescription("Member to mute").setRequired(true))
    .addIntegerOption(o => o.setName("minutes").setDescription("Duration in minutes (0 = permanent)").setMinValue(0))
    .addStringOption(o => o.setName("reason").setDescription("Reason for mute")),

  new SlashCommandBuilder()
    .setName("unmute").setDescription("Unmute a member")
    .addUserOption(o => o.setName("user").setDescription("Member to unmute").setRequired(true)),

  new SlashCommandBuilder()
    .setName("kick").setDescription("Kick a member")
    .addUserOption(o => o.setName("user").setDescription("Member to kick").setRequired(true))
    .addStringOption(o => o.setName("reason").setDescription("Reason for kick")),

  new SlashCommandBuilder()
    .setName("ban").setDescription("Ban a member")
    .addUserOption(o => o.setName("user").setDescription("Member to ban").setRequired(true))
    .addStringOption(o => o.setName("reason").setDescription("Reason for ban"))
    .addIntegerOption(o => o.setName("delete_days").setDescription("Days of messages to delete (0–7)").setMinValue(0).setMaxValue(7)),

  new SlashCommandBuilder()
    .setName("unban").setDescription("Unban a user by ID")
    .addStringOption(o => o.setName("user_id").setDescription("The user ID to unban").setRequired(true))
    .addStringOption(o => o.setName("reason").setDescription("Reason for unban")),

  new SlashCommandBuilder()
    .setName("purge").setDescription("Bulk-delete messages in this channel")
    .addIntegerOption(o => o.setName("count").setDescription("Number of messages (1–100)").setRequired(true).setMinValue(1).setMaxValue(100))
    .addUserOption(o => o.setName("user").setDescription("Only delete messages from this user (optional)")),

  new SlashCommandBuilder()
    .setName("lockdown").setDescription("Lock or unlock a channel")
    .addStringOption(o =>
      o.setName("action").setDescription("Lock or unlock").setRequired(true)
        .addChoices({ name: "lock", value: "lock" }, { name: "unlock", value: "unlock" }))
    .addChannelOption(o => o.setName("channel").setDescription("Channel to lock/unlock (defaults to current)")),

  new SlashCommandBuilder()
    .setName("panic").setDescription("EMERGENCY: lock every text channel at once (owner only)"),

  new SlashCommandBuilder()
    .setName("warn").setDescription("Issue a warning to a member")
    .addUserOption(o => o.setName("user").setDescription("Member to warn").setRequired(true))
    .addStringOption(o => o.setName("reason").setDescription("Reason for warning")),

  new SlashCommandBuilder()
    .setName("warnings").setDescription("View a member's warnings")
    .addUserOption(o => o.setName("user").setDescription("Member to inspect").setRequired(true)),

  new SlashCommandBuilder()
    .setName("clearwarns").setDescription("Clear all warnings for a member")
    .addUserOption(o => o.setName("user").setDescription("Member to clear").setRequired(true)),

  new SlashCommandBuilder()
    .setName("config").setDescription("View Guardian configuration (bot owner only)"),

  new SlashCommandBuilder()
    .setName("nuketest").setDescription("Confirm anti-nuke system is active (owner only)"),

  new SlashCommandBuilder()
    .setName("limits").setDescription("Check your remaining mod action limits for today"),

  // ── Anti-Ping (customizable) ──
  new SlashCommandBuilder()
    .setName("antiping").setDescription("Configure anti-ping protection for staff/VIPs")
    .addSubcommand(s => s.setName("status").setDescription("Show current anti-ping settings"))
    .addSubcommand(s => s.setName("toggle").setDescription("Enable or disable anti-ping")
      .addBooleanOption(o => o.setName("enabled").setDescription("On or off").setRequired(true)))
    .addSubcommand(s => s.setName("action").setDescription("Set punishment for pinging a protected target")
      .addStringOption(o => o.setName("type").setDescription("Punishment").setRequired(true)
        .addChoices(
          { name: "none (log only)",   value: "none"    },
          { name: "warn",              value: "warn"    },
          { name: "mute (mute role)",  value: "mute"    },
          { name: "timeout (native)",  value: "timeout" },
        )))
    .addSubcommand(s => s.setName("duration").setDescription("Mute/timeout duration in minutes")
      .addIntegerOption(o => o.setName("minutes").setDescription("Minutes").setRequired(true).setMinValue(1).setMaxValue(40320)))
    .addSubcommand(s => s.setName("delete").setDescription("Delete the offending message?")
      .addBooleanOption(o => o.setName("enabled").setDescription("True to delete").setRequired(true)))
    .addSubcommand(s => s.setName("ignorereplies").setDescription("Ignore reply-pings?")
      .addBooleanOption(o => o.setName("enabled").setDescription("True to ignore reply pings").setRequired(true)))
    .addSubcommand(s => s.setName("response").setDescription("Customize the warning message — {user} {targets} {action}")
      .addStringOption(o => o.setName("text").setDescription("Template text, or 'default' to reset").setRequired(true)))
    .addSubcommand(s => s.setName("notify").setDescription("Post the public warning message in the channel?")
      .addBooleanOption(o => o.setName("enabled").setDescription("True to post warning in channel").setRequired(true)))
    .addSubcommand(s => s.setName("protect").setDescription("Add/remove a protected user")
      .addStringOption(o => o.setName("action").setDescription("add or remove").setRequired(true)
        .addChoices({ name: "add", value: "add" }, { name: "remove", value: "remove" }))
      .addUserOption(o => o.setName("user").setDescription("User to protect").setRequired(true)))
    .addSubcommand(s => s.setName("protectrole").setDescription("Add/remove a protected role")
      .addStringOption(o => o.setName("action").setDescription("add or remove").setRequired(true)
        .addChoices({ name: "add", value: "add" }, { name: "remove", value: "remove" }))
      .addRoleOption(o => o.setName("role").setDescription("Role to protect").setRequired(true)))
    .addSubcommand(s => s.setName("list").setDescription("List protected users and roles")),

  new SlashCommandBuilder()
    .setName("setup").setDescription("Configure Guardian for this server")
    .addSubcommand(s => s.setName("quick").setDescription("Auto-provision a Muted role + log/alert/message-log channels in one step")
      .addRoleOption(o => o.setName("mod_role").setDescription("Role allowed to use moderation commands (optional)")))
    .addSubcommand(s => s.setName("view").setDescription("Show current configuration for this server"))
    .addSubcommand(s => s.setName("roles").setDescription("Set the mod role and/or mute role")
      .addRoleOption(o => o.setName("mod_role").setDescription("Role allowed to use moderation commands"))
      .addRoleOption(o => o.setName("mute_role").setDescription("Role applied on mute (must deny Send Messages)")))
    .addSubcommand(s => s.setName("channels").setDescription("Set log/alert/message-log channels")
      .addChannelOption(o => o.setName("log_channel").setDescription("Security log channel").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
      .addChannelOption(o => o.setName("alert_channel").setDescription("Critical-alert channel (owner pinged)").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
      .addChannelOption(o => o.setName("msg_log_channel").setDescription("Deleted / edited message log channel").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)))
    .addSubcommand(s => s.setName("whitelist").setDescription("Add/remove an anti-nuke whitelist entry")
      .addStringOption(o => o.setName("action").setDescription("add or remove").setRequired(true)
        .addChoices({ name: "add", value: "add" }, { name: "remove", value: "remove" }))
      .addUserOption(o => o.setName("user").setDescription("User to whitelist"))
      .addRoleOption(o => o.setName("role").setDescription("Role to whitelist")))
    .addSubcommand(s => s.setName("failsafe").setDescription("Add/remove a role targeted by !failsafe")
      .addStringOption(o => o.setName("action").setDescription("add or remove").setRequired(true)
        .addChoices({ name: "add", value: "add" }, { name: "remove", value: "remove" }))
      .addRoleOption(o => o.setName("role").setDescription("Role to add/remove from the failsafe target list").setRequired(true))),

  new SlashCommandBuilder().setName("help").setDescription("Show all Guardian Bot commands"),
];

// ── Register Commands (GLOBAL — one registration serves every server, present
//    and future; propagation can take up to ~1h, like Wick/large bots) ──
const commandBody = () => commands.map(c => c.toJSON());
async function registerCommandsGlobal() {
  const rest = new REST({ version: "10" }).setToken(TOKEN);
  try {
    console.log("🔄 Registering global slash commands…");
    await rest.put(Routes.applicationCommands(CLIENT_ID), { body: commandBody() });
    console.log("✅ Global commands registered (available in every server; new servers may take up to ~1h).");
  } catch (e) { console.error("❌ Global command registration failed:", e.message); }
}

// ── Embed Helpers ─────────────────────────────────────────────
const COLORS = {
  success: 0x00e5a0, warn: 0xf5a623, danger: 0xff3b5c, info: 0x5865f2,
  muted: 0xff7518, nuke: 0xff0033, neutral: 0x2f3136,
};

function embed(color, description, title = null) {
  const e = new EmbedBuilder().setColor(color).setDescription(description).setTimestamp();
  if (title) e.setTitle(`🛡️ ${title}`);
  return e;
}

async function secLog(guild, title, desc, color = COLORS.success) {
  appendForensic(guild.id, "log", { title, desc });
  const logId = gc(guild).logChannelId;
  if (!logId) return;
  const ch = guild.channels.cache.get(logId);
  if (!ch) return;
  ch.send({ embeds: [embed(color, desc, title)] }).catch(() => {});
}

// Critical alert: forensic trail + channel ping + owner DM (so a nuked log channel can't blind the owner).
function alertOwner(guild, desc, color = COLORS.nuke, title = "Security Alert") {
  appendForensic(guild.id, "alert", { title, desc });
  const g = gc(guild);
  const chId = g.alertChannelId || g.logChannelId;
  const ch = chId ? guild.channels.cache.get(chId) : null;
  if (ch) ch.send({
    content: `<@${BOT_OWNER_ID}>`,
    embeds: [embed(color, desc, title)],
    allowedMentions: { users: [BOT_OWNER_ID] },
  }).catch(() => {});
  if (config.ownerDM)
    client.users.fetch(BOT_OWNER_ID)
      .then(u => u.send({ embeds: [embed(color, `**[${guild.name}]** ${desc}`, title)] }))
      .catch(() => {});
}

function isOwner(idOrMember) {
  const id = typeof idOrMember === "string" ? idOrMember : idOrMember?.id;
  return id === BOT_OWNER_ID;
}

function isMod(member) {
  if (!member) return false;
  if (isOwner(member)) return true;
  if (member.id === member.guild.ownerId) return true;
  const modRoleId = gc(member.guild).modRoleId;
  if (!modRoleId) return false;
  return member.roles.cache.has(modRoleId);
}

function isWhitelisted(member) {
  if (!member) return false;
  if (isOwner(member)) return true;                       // hardcoded owner is always immune
  if (member.id === member.guild.ownerId) return true;
  const g = gc(member.guild);
  if (g.nukeWhitelistUserIds.includes(member.id)) return true;
  return member.roles.cache.some(r => g.nukeWhitelistRoleIds.includes(r.id));
}

// Best-effort DM to a member before punitive action.
async function tryDM(user, text) {
  try { await user.send(text); } catch (_) {}
}

// Guard: can `actor` moderate `target`? Protects owner/whitelist and respects hierarchy.
function canActOn(actor, target) {
  if (!target) return { ok: false, why: "❌ User not found in this server." };
  if (isOwner(target)) return { ok: false, why: "❌ That user is the bot owner — protected." };
  if (target.id === target.guild.ownerId) return { ok: false, why: "❌ That user is the server owner — protected." };
  if (isWhitelisted(target)) return { ok: false, why: "❌ That user is whitelisted — protected." };
  if (target.id === actor.id) return { ok: false, why: "❌ You can't action yourself." };
  const me = target.guild.members.me;
  if (me && target.roles.highest.position >= me.roles.highest.position)
    return { ok: false, why: "❌ That user's highest role is above mine — fix my role position." };
  const actorPrivileged = isOwner(actor) || actor.id === actor.guild.ownerId;
  if (!actorPrivileged && target.roles.highest.position >= actor.roles.highest.position)
    return { ok: false, why: "❌ That user's role is equal to or higher than yours." };
  return { ok: true };
}

// ── Mod Rate Limit Helpers (scoped per guild — a mod's limits in one server
//    are independent of their activity in any other) ───────────────────────
function getModEntry(guildId, userId) {
  const key = `${guildId}:${userId}`;
  if (!modRateTracker.has(key)) {
    modRateTracker.set(key, { bans: [], kicks: [], mutes: [], purges: [], lockdowns: [], warns: [] });
  }
  return modRateTracker.get(key);
}
function pruneWindow(arr, windowMs = config.modWindowMs) {
  const cutoff = Date.now() - windowMs;
  return arr.filter(t => t > cutoff);
}
function checkModLimit(guildId, memberId, action) {
  const entry = getModEntry(guildId, memberId);
  const limitKey = `mod${action.charAt(0).toUpperCase() + action.slice(1)}Limit`;
  const limit = config[limitKey];
  entry[`${action}s`] = pruneWindow(entry[`${action}s`]);
  const used = entry[`${action}s`].length;
  const allowed = used < limit;
  let resetsInMin = 0;
  if (!allowed && entry[`${action}s`].length > 0) {
    const oldest = entry[`${action}s`][0];
    resetsInMin = Math.ceil((oldest + config.modWindowMs - Date.now()) / 60000);
  }
  return { allowed, used, limit, remaining: limit - used, resetsInMin };
}
function recordModAction(guildId, memberId, action) {
  const entry = getModEntry(guildId, memberId);
  entry[`${action}s`] = pruneWindow(entry[`${action}s`]);
  entry[`${action}s`].push(Date.now());
}
function limitDeniedEmbed(action, used, limit, resetsInMin) {
  return embed(COLORS.danger,
    `⛔ **Rate limit reached for \`/${action}\`**\n\n` +
    `You've used **${used}/${limit}** ${action}s in the past ${config.modWindowMs / 3600000}h.\n` +
    `⏱️ Resets in **~${resetsInMin} minute${resetsInMin === 1 ? "" : "s"}**.`);
}
function usageFooter(action, used, limit) {
  const remaining = limit - used;
  const bar = buildBar(used, limit, 10);
  const warning = remaining <= Math.ceil(limit * 0.2) && remaining > 0
    ? `\n⚠️ Only **${remaining}** ${action}${remaining === 1 ? "" : "s"} remaining today.` : "";
  return `\`${bar}\` **${used}/${limit}** ${action}s used today${warning}`;
}
function buildBar(used, limit, width = 10) {
  const filled = Math.min(width, Math.round((used / Math.max(limit, 1)) * width));
  return "█".repeat(filled) + "░".repeat(width - filled);
}

// setTimeout overflows (and fires immediately) past ~24.8 days. Chunk long delays.
const MAX_TIMEOUT = 2147483647;
function scheduleTask(fn, delayMs) {
  if (delayMs <= MAX_TIMEOUT) return setTimeout(fn, Math.max(0, delayMs));
  return setTimeout(() => scheduleTask(fn, delayMs - MAX_TIMEOUT), MAX_TIMEOUT);
}

// ── Mute Utility (strips + stashes roles, restores on unmute) ─
async function muteUser(member, durationMin, reason) {
  const muteRoleId = gc(member.guild).muteRoleId;
  if (!muteRoleId) return false;
  const muteRole = member.guild.roles.cache.get(muteRoleId);
  if (!muteRole) return false;

  const removable = member.roles.cache.filter(r =>
    r.id !== member.guild.id && r.id !== muteRole.id && !r.managed && r.editable);
  const strippedIds = [...removable.keys()];

  const unstrippable = member.roles.cache.filter(r =>
    r.id !== member.guild.id && r.id !== muteRole.id && (r.managed || !r.editable));

  try {
    if (strippedIds.length) await member.roles.remove(strippedIds, `Mute: stash roles — ${reason}`);
    await member.roles.add(muteRole, reason);
  } catch (e) { console.error("⚠️ mute role op failed:", e.message); }

  if (!mutedRoles[member.guild.id]) mutedRoles[member.guild.id] = {};
  const prior = mutedRoles[member.guild.id][member.id]?.roles || [];
  mutedRoles[member.guild.id][member.id] = {
    roles:     [...new Set([...prior, ...strippedIds])],
    reason, mutedAt: Date.now(),
    expiresAt: durationMin > 0 ? Date.now() + durationMin * 60000 : null,
  };
  saveMutedRoles(member.guild.id);

  const stash = mutedRoles[member.guild.id][member.id].roles;
  secLog(member.guild, "Member Muted",
    `<@${member.id}> muted for **${durationMin > 0 ? durationMin + " min" : "permanent"}** — ${reason}\n` +
    `📦 Stashed **${stash.length}** role${stash.length === 1 ? "" : "s"}: ${stash.length ? stash.map(id => `<@&${id}>`).join(", ") : "none"}` +
    (unstrippable.size ? `\n⚠️ Couldn't strip (managed / above bot): ${unstrippable.map(r => `<@&${r.id}>`).join(", ")}` : ""),
    COLORS.muted);

  if (durationMin > 0) {
    scheduleTask(() => unmuteUser(member.guild, member.id, "Auto-unmute (timer)"), durationMin * 60000);
  }
  return true;
}

// ── Unmute Utility (removes mute role + restores stashed roles) ─
async function unmuteUser(guild, userId, reason = "Unmute") {
  const muteRoleId = gc(guild).muteRoleId;
  const muteRole = muteRoleId ? guild.roles.cache.get(muteRoleId) : null;
  const member   = await guild.members.fetch(userId).catch(() => null);
  const stash    = mutedRoles[guild.id]?.[userId];

  if (member) {
    if (muteRole && member.roles.cache.has(muteRole.id))
      await member.roles.remove(muteRole, reason).catch(() => {});

    if (stash?.roles?.length) {
      const restorable = stash.roles.filter(id => {
        const r = guild.roles.cache.get(id);
        return r && r.editable && !r.managed;
      });
      const lost = stash.roles.filter(id => !restorable.includes(id));
      if (restorable.length) await member.roles.add(restorable, `Restore stashed roles — ${reason}`).catch(() => {});
      secLog(guild, "Roles Restored",
        `<@${userId}> unmuted — restored **${restorable.length}** role${restorable.length === 1 ? "" : "s"}: ${restorable.length ? restorable.map(id => `<@&${id}>`).join(", ") : "none"}` +
        (lost.length ? `\n⚠️ Could not restore (deleted / above bot): ${lost.map(id => `<@&${id}>`).join(", ")}` : "") +
        `\n_(${reason})_`, COLORS.success);
    } else {
      secLog(guild, "Member Unmuted", `<@${userId}> unmuted — no stashed roles to restore. _(${reason})_`);
    }
  }

  if (mutedRoles[guild.id]) {
    delete mutedRoles[guild.id][userId];
    if (!Object.keys(mutedRoles[guild.id]).length) delete mutedRoles[guild.id];
    saveMutedRoles(guild.id);
  }
}

// ── Boot recovery: re-apply / reschedule / expire mutes ───────
async function recoverMutes() {
  for (const [guildId, users] of Object.entries(mutedRoles)) {
    const guild = client.guilds.cache.get(guildId);
    if (!guild) continue;
    const muteRoleId = gc(guild).muteRoleId;
    const muteRole = muteRoleId ? guild.roles.cache.get(muteRoleId) : null;

    for (const [userId, data] of Object.entries(users)) {
      // Re-apply the mute role if it was lost during downtime (still-muted members).
      if (muteRole) {
        const m = await guild.members.fetch(userId).catch(() => null);
        if (m && !m.roles.cache.has(muteRole.id) && (data.expiresAt == null || data.expiresAt > Date.now())) {
          m.roles.add(muteRole, "Re-apply mute (recovered after restart)").catch(() => {});
        }
      }
      if (data.expiresAt == null) continue; // permanent — leave for manual /unmute
      const remaining = data.expiresAt - Date.now();
      if (remaining <= 0) unmuteUser(guild, userId, "Auto-unmute (expired during downtime)");
      else scheduleTask(() => unmuteUser(guild, userId, "Auto-unmute (timer, resumed post-restart)"), remaining);
    }
  }
}

// ── Anti-Spam ─────────────────────────────────────────────────
function checkSpam(message) {
  if (!message.member) return false;
  if (config.spamExemptStaff && (isMod(message.member) || isWhitelisted(message.member))) return false; // set SPAM_EXEMPT_STAFF=false to test on your own account
  const uid = message.author.id;
  const key = `${message.guild.id}:${uid}`;
  const now = Date.now();

  // Mass-mention in a single message (@everyone / @here counts as mass)
  const mentionCount = message.mentions.users.size + message.mentions.roles.size +
    (message.mentions.everyone ? config.spamMentionLimit : 0);
  if (mentionCount >= config.spamMentionLimit) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, `Anti-spam: mass mention (${mentionCount})`);
    secLog(message.guild, "Anti-Spam", `<@${uid}> mass-mentioned (${mentionCount}) in <#${message.channel.id}> → muted`, COLORS.warn);
    return true;
  }

  // Scam / phishing / IP-grabber links
  if (config.scamBlock && SCAM_RE.test(message.content)) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, "Anti-spam: scam/grabber link");
    alertOwner(message.guild, `🎣 <@${uid}> posted a suspected **scam/grabber link** in <#${message.channel.id}> → deleted + muted.`, COLORS.danger, "Scam Link Blocked");
    return true;
  }

  // Invite-link spam
  if (config.spamBlockInvites && INVITE_RE.test(message.content) && !isMod(message.member)) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, "Anti-spam: posted invite link");
    secLog(message.guild, "Anti-Spam", `<@${uid}> posted an invite link in <#${message.channel.id}> → muted`, COLORS.warn);
    return true;
  }

  // Duplicate-message flood
  const dupe = dupeTracker.get(key);
  if (dupe && dupe.content === message.content && now - dupe.ts < config.spamWindowMs * 3) {
    dupe.count++; dupe.ts = now;
    if (dupe.count >= config.spamDuplicateLimit) {
      message.delete().catch(() => {});
      muteUser(message.member, config.spamMuteMin, "Anti-spam: duplicate flood");
      dupeTracker.set(key, { content: "", count: 0, ts: now });
      secLog(message.guild, "Anti-Spam", `<@${uid}> duplicate-flooded in <#${message.channel.id}> → muted`, COLORS.warn);
      return true;
    }
  } else {
    dupeTracker.set(key, { content: message.content, count: 1, ts: now });
  }

  // Frequency flood
  const arr = (spamTracker.get(key) || []).filter(t => now - t < config.spamWindowMs);
  arr.push(now);
  spamTracker.set(key, arr);
  if (arr.length >= config.spamThreshold) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, "Anti-spam: message flood");
    spamTracker.set(key, []);
    secLog(message.guild, "Anti-Spam", `<@${uid}> message-flooded in <#${message.channel.id}> → muted`, COLORS.warn);
    return true;
  }
  return false;
}

// ── Anti-Ping ─────────────────────────────────────────────────
function renderAntiPingResponse(a, memberId, targets, actionText) {
  return a.responseTemplate
    .split("{user}").join(`<@${memberId}>`)
    .split("{targets}").join(targets)
    .split("{action}").join(actionText);
}
async function checkAntiPing(message) {
  const a = ap(message.guild);
  if (!a.enabled) return;
  const member = message.member;
  if (!member) return;
  if (member.id === message.guild.ownerId) return;
  if (isMod(member) || isWhitelisted(member)) return;

  const hits = new Set();
  for (const [id, user] of message.mentions.users) {
    if (id === message.author.id || user.bot) continue;
    if (a.ignoreReplies && message.mentions.repliedUser?.id === id) continue;
    if (a.protectedUsers.includes(id)) { hits.add(`<@${id}>`); continue; }
    const t = message.guild.members.cache.get(id);
    if (t && t.roles.cache.some(r => a.protectedRoles.includes(r.id))) hits.add(`<@${id}>`);
  }
  for (const [id] of message.mentions.roles) {
    if (a.protectedRoles.includes(id)) hits.add(`<@&${id}>`);
  }
  if (hits.size === 0) return;

  const targets = [...hits].join(", ");
  const reason  = `Anti-ping: mentioned protected ${targets}`;
  if (a.deleteMessage) message.delete().catch(() => {});

  let actionText = "logged only";
  switch (a.action) {
    case "mute":    await muteUser(member, a.timeoutMin, reason); actionText = `muted for ${a.timeoutMin} min`; break;
    case "timeout": await member.timeout(a.timeoutMin * 60000, reason).catch(() => {}); actionText = `timed out for ${a.timeoutMin} min`; break;
    case "warn":    actionText = "warned"; break;
  }
  if (a.notifyChannel) {
    message.channel.send({ embeds: [embed(COLORS.warn, renderAntiPingResponse(a, member.id, targets, actionText), "Anti-Ping")] })
      .then(m => setTimeout(() => m.delete().catch(() => {}), 8000)).catch(() => {});
  }
  secLog(message.guild, "📡 Anti-Ping Triggered",
    `<@${member.id}> pinged ${targets} in <#${message.channel.id}> → **${actionText}**`, COLORS.warn);
}

// ── Anti-Raid (join velocity + new-account quarantine) ────────
client.on(Events.GuildMemberAdd, async (member) => {
  const now = Date.now();
  const gid = member.guild.id;

  // Quarantine brand-new accounts that join while THIS guild's raid lockdown is active.
  if (isLockdown(gid) && config.raidKickNewOnLock && !member.user.bot) {
    const ageMin = (now - member.user.createdTimestamp) / 60000;
    if (ageMin < config.raidMinAccountAgeMin) {
      await tryDM(member.user, "The server is in raid lockdown. Please rejoin later.");
      await member.kick(`Raid lockdown: new account (${Math.round(ageMin)}m old)`).catch(() => {});
      secLog(member.guild, "Raid Quarantine", `Kicked new account <@${member.id}> (${Math.round(ageMin)}m old) during lockdown.`, COLORS.danger);
      return;
    }
  }

  const joins = (joinTracker.get(gid) || []).filter(t => now - t < config.raidWindowMs);
  joins.push(now);
  joinTracker.set(gid, joins);
  const recent = joins.length;
  if (recent >= config.raidJoinThreshold && !isLockdown(gid)) {
    setLockdown(gid);
    alertOwner(member.guild, `🚨 **RAID DETECTED** — **${recent}** joins in ${config.raidWindowMs / 1000}s. Server locked down for **${config.raidLockdownMin} min**.`, COLORS.nuke, "RAID DETECTED");
    member.guild.channels.cache.forEach(ch => {
      if (ch.isTextBased() && !ch.isThread()) ch.permissionOverwrites.edit(member.guild.roles.everyone, { SendMessages: false }).catch(() => {});
    });
    setTimeout(() => {
      member.guild.channels.cache.forEach(ch => {
        if (ch.isTextBased() && !ch.isThread()) ch.permissionOverwrites.edit(member.guild.roles.everyone, { SendMessages: null }).catch(() => {});
      });
      clearLockdown(gid);
      secLog(member.guild, "Lockdown Lifted", `Auto-lifted after **${config.raidLockdownMin} minutes**.`);
    }, config.raidLockdownMin * 60000);
  }
});

// ── Anti-Nuke engine (scoped per guild — a user's actions in one server never
//    count toward thresholds in another) ───────────────────────────────────
function getNukeEntry(guildId, userId) {
  const key = `${guildId}:${userId}`;
  if (!nukeTracker.has(key)) nukeTracker.set(key, {});
  return nukeTracker.get(key);
}
function pruneOld(arr) {
  return (arr || []).filter(t => Date.now() - t < config.nukeWindowMs);
}
// Push a timestamp under `key`; returns true if the threshold is reached.
function bump(guildId, userId, key, threshold) {
  const entry = getNukeEntry(guildId, userId);
  entry[key] = pruneOld(entry[key]);
  entry[key].push(Date.now());
  return entry[key].length >= threshold;
}
function resetBump(guildId, userId, key) {
  const entry = getNukeEntry(guildId, userId);
  entry[key] = [];
}

async function nukeResponse(guild, member, reason) {
  // Re-guard: never punish owner/whitelisted, even if reached here.
  if (!member || isWhitelisted(member)) return;

  alertOwner(guild,
    `**Executor:** <@${member.id}> (\`${member.id}\`)\n**Reason:** ${reason}\n**Action:** dangerous roles stripped → ban attempted.`,
    COLORS.nuke, "🔴 ANTI-NUKE TRIGGERED");

  try {
    const toRemove = member.roles.cache.filter(r => r.permissions.any(DANGER_PERMS) && r.editable);
    if (toRemove.size > 0) await member.roles.remove([...toRemove.keys()], "Anti-nuke: role strip");
  } catch (e) {
    secLog(guild, "Anti-Nuke", `⚠️ Could not strip roles from <@${member.id}>: ${e.message}`, COLORS.warn);
  }

  try {
    await member.ban({ reason: `Anti-Nuke: ${reason}` });
    secLog(guild, "Anti-Nuke", `✅ Banned <@${member.id}> — ${reason}`, COLORS.nuke);
  } catch (e) {
    // Ban failed (likely above the bot). Try kick; otherwise leave de-permed + escalate.
    const kicked = await member.kick(`Anti-Nuke: ${reason}`).catch(() => null);
    alertOwner(guild,
      `⚠️ Could not ban <@${member.id}> (${e.message}). ` +
      (kicked === null ? `Kick also failed — roles stripped only. **Check my role position immediately.**` : `Kicked instead.`),
      COLORS.danger, "Anti-Nuke — manual review");
  }

  // Nuke-storm escalation: several responses in THIS guild in a short window ⇒ lock it down.
  if (bumpStorm(guild.id)) {
    serverEmergencyLock(guild, `${config.nukeStormThreshold}+ nuke responses within ${config.nukeStormWindowMs / 1000}s`);
  }
}

// Unified detector: fires once per real audit-log entry with a reliable executor.
// Bot-executed actions (i.e. our own commands) are skipped so command paths
// remain the single counter for command-driven floods (no double counting).
client.on(Events.GuildAuditLogEntryCreate, async (entry, guild) => {
  try {
    const { action, executorId, targetId } = entry;
    if (!executorId || executorId === client.user.id) return;
    const executor = await guild.members.fetch(executorId).catch(() => null);
    if (!executor || isWhitelisted(executor)) return;

    switch (action) {
      case AuditLogEvent.ChannelDelete:
        if (bump(guild.id, executorId, "chDel", config.nukeChannelThreshold)) {
          resetBump(guild.id, executorId, "chDel");
          return nukeResponse(guild, executor, `Deleted ${config.nukeChannelThreshold}+ channels in ${config.nukeWindowMs / 1000}s`);
        }
        break;

      case AuditLogEvent.ChannelCreate:
        if (bump(guild.id, executorId, "chCreate", config.nukeChannelCreateThresh)) {
          resetBump(guild.id, executorId, "chCreate");
          return nukeResponse(guild, executor, `Created ${config.nukeChannelCreateThresh}+ channels in ${config.nukeWindowMs / 1000}s`);
        }
        break;

      case AuditLogEvent.RoleDelete:
        if (bump(guild.id, executorId, "roleDel", config.nukeRoleThreshold)) {
          resetBump(guild.id, executorId, "roleDel");
          return nukeResponse(guild, executor, `Deleted ${config.nukeRoleThreshold}+ roles in ${config.nukeWindowMs / 1000}s`);
        }
        break;

      case AuditLogEvent.RoleCreate:
        if (bump(guild.id, executorId, "roleCreate", config.nukeRoleCreateThresh)) {
          resetBump(guild.id, executorId, "roleCreate");
          return nukeResponse(guild, executor, `Created ${config.nukeRoleCreateThresh}+ roles in ${config.nukeWindowMs / 1000}s`);
        }
        break;

      case AuditLogEvent.MemberBanAdd:
        if (bump(guild.id, executorId, "bans", config.nukeBanThreshold)) {
          resetBump(guild.id, executorId, "bans");
          return nukeResponse(guild, executor, `Issued ${config.nukeBanThreshold}+ bans in ${config.nukeWindowMs / 1000}s`);
        }
        break;

      case AuditLogEvent.MemberKick:
      case AuditLogEvent.MemberPrune:
        if (bump(guild.id, executorId, "kicks", config.nukeKickThreshold)) {
          resetBump(guild.id, executorId, "kicks");
          return nukeResponse(guild, executor, `Removed ${config.nukeKickThreshold}+ members in ${config.nukeWindowMs / 1000}s`);
        }
        break;

      case AuditLogEvent.WebhookCreate:
        if (bump(guild.id, executorId, "webhooks", config.nukeWebhookThreshold)) {
          resetBump(guild.id, executorId, "webhooks");
          const chId = entry.changes?.find(c => c.key === "channel_id")?.new || entry.extra?.channel?.id;
          const channel = chId ? guild.channels.cache.get(chId) : null;
          const hooks = channel ? await channel.fetchWebhooks().catch(() => null) : null;
          if (hooks) for (const wh of hooks.filter(w => w.owner?.id === executorId).values())
            await wh.delete("Anti-nuke: webhook abuse").catch(() => {});
          return nukeResponse(guild, executor, `Created ${config.nukeWebhookThreshold}+ webhooks in ${config.nukeWindowMs / 1000}s`);
        }
        break;

      case AuditLogEvent.RoleUpdate: {
        const permChange = entry.changes?.find(c => c.key === "permissions");
        if (!permChange) break;
        const oldP = new PermissionsBitField(BigInt(permChange.old || 0));
        const newP = new PermissionsBitField(BigInt(permChange.new || 0));
        const escalated = DANGER_PERMS.some(p => !oldP.has(p) && newP.has(p));
        if (!escalated) break;
        const role = guild.roles.cache.get(targetId);
        if (role && role.editable) await role.setPermissions(oldP, "Anti-nuke: revert perm escalation").catch(() => {});
        alertOwner(guild, `⚠️ <@${executorId}> granted dangerous permissions to <@&${targetId}>. **Reverted.**`, COLORS.warn, "Permission Escalation Blocked");
        if (bump(guild.id, executorId, "permEsc", 3)) { resetBump(guild.id, executorId, "permEsc"); return nukeResponse(guild, executor, "Repeated permission escalation"); }
        break;
      }

      case AuditLogEvent.MemberRoleUpdate: {
        const added = entry.changes?.find(c => c.key === "$add")?.new || [];
        const dangerous = added.filter(r => {
          const role = guild.roles.cache.get(r.id);
          return role && role.permissions.any(DANGER_PERMS);
        });
        if (!dangerous.length) break;
        const target = await guild.members.fetch(targetId).catch(() => null);
        if (target) await target.roles.remove(dangerous.map(r => r.id), "Anti-nuke: revert dangerous role grant").catch(() => {});
        alertOwner(guild, `⚠️ <@${executorId}> granted dangerous role(s) ${dangerous.map(r => `<@&${r.id}>`).join(", ")} to <@${targetId}>. **Reverted.**`, COLORS.warn, "Privilege Grant Blocked");
        if (bump(guild.id, executorId, "dangerGrant", config.nukeMemberRoleThreshold)) {
          resetBump(guild.id, executorId, "dangerGrant");
          return nukeResponse(guild, executor, `Granted dangerous roles ${config.nukeMemberRoleThreshold}+ times in ${config.nukeWindowMs / 1000}s`);
        }
        break;
      }

      case AuditLogEvent.BotAdd: {
        const added = await guild.members.fetch(targetId).catch(() => null);
        if (config.nukeBotAddAction === "kick" && added && added.kickable)
          await added.kick("Anti-nuke: unauthorized bot add").catch(() => {});

        // Strip EVERY removable role from whoever added the bot.
        // (Skips @everyone, managed/integration roles, and anything above my top role.)
        const removable = executor.roles.cache.filter(r =>
          r.id !== guild.id && !r.managed && r.editable);
        const strippedIds = [...removable.keys()];
        if (strippedIds.length)
          await executor.roles.remove(strippedIds, "Anti-nuke: added a bot — roles stripped").catch(() => {});

        const unstrippable = executor.roles.cache.filter(r =>
          r.id !== guild.id && (r.managed || !r.editable));

        alertOwner(guild,
          `⚠️ <@${executorId}> added bot <@${targetId}> — ${config.nukeBotAddAction === "kick" ? "**bot removed.**" : "review required."}\n` +
          `🧹 Stripped **${strippedIds.length}** role${strippedIds.length === 1 ? "" : "s"} from <@${executorId}>: ${strippedIds.length ? strippedIds.map(id => `<@&${id}>`).join(", ") : "none"}` +
          (unstrippable.size ? `\n⚠️ Couldn't strip (managed / above me): ${unstrippable.map(r => `<@&${r.id}>`).join(", ")}` : ""),
          COLORS.danger, "Bot Added");
        break;
      }

      case AuditLogEvent.EmojiDelete:
      case AuditLogEvent.StickerDelete:
        if (bump(guild.id, executorId, "emojiDel", config.nukeEmojiThreshold)) {
          resetBump(guild.id, executorId, "emojiDel");
          return nukeResponse(guild, executor, `Deleted ${config.nukeEmojiThreshold}+ emojis/stickers in ${config.nukeWindowMs / 1000}s`);
        }
        break;

      case AuditLogEvent.GuildUpdate:
        alertOwner(guild, `⚙️ Server settings were changed by <@${executorId}>. Review the audit log.`, COLORS.warn, "Guild Settings Changed");
        break;
    }
  } catch (e) {
    console.error("⚠️ audit-log handler error:", e.message);
  }
});

// ── Messages: anti-spam + anti-ping ───────────────────────────
client.on(Events.MessageCreate, (message) => {
  if (message.author.bot || !message.guild) return;
  if (checkSpam(message)) return;
  checkAntiPing(message);
});

// ── Hidden owner-only commands (never registered as slash → not shown in /) ──
client.on(Events.MessageCreate, async (message) => {
  if (!message.guild || message.author.id !== BOT_OWNER_ID) return;
  const cmd = message.content.trim().toLowerCase();
  try {
    if (cmd === "!failsafe") return await runFailsafe(message);
    if (cmd === "!restore")  return await runRestore(message);
    if (cmd === "!snapshot") {
      const r = snapshotGuild(message.guild);
      const kept = (snapshots[message.guild.id] || []).length;
      return message.reply(`📸 Snapshot saved — **${r.roles}** roles, **${r.channels}** channels. (${kept}/${config.snapshotMax} kept)`);
    }
    if (cmd === "!snapshots") {
      const arr = snapshots[message.guild.id] || [];
      if (!arr.length) return message.reply("No snapshots yet. Run `!snapshot`.");
      const lines = arr.map((s, i) => `**${i + 1}.** <t:${Math.floor(s.takenAt / 1000)}:R> — ${s.roles.length} roles, ${s.channels.length} channels`).join("\n");
      return message.reply(`📸 **Snapshots (newest last):**\n${lines}`);
    }
    if (cmd === "!rollback") return await rollbackGuild(message.guild, message);
    if (cmd === "!ownerhelp") {
      return message.reply(
        "🛡️ **Hidden owner commands** (only you can run these):\n" +
        "`!failsafe` — back up + delete the target roles and kick all bots\n" +
        "`!restore` — rebuild those roles (perms, position, channel access, members)\n" +
        "`!snapshot` — take a full-guild snapshot now\n" +
        "`!snapshots` — list stored snapshots\n" +
        "`!rollback` — rebuild missing roles & channels from the latest snapshot");
    }
  } catch (e) {
    console.error("⚠️ owner command failed:", e.message);
    message.reply(`⚠️ Command errored: ${e.message}`).catch(() => {});
  }
});

// ── Deleted-message + image logging ───────────────────────────
client.on(Events.MessageDelete, async (message) => {
  try {
    if (!message.guild) return;
    const msgLogId = gc(message.guild).msgLogChannelId;
    if (!msgLogId) return;
    if (message.channelId === msgLogId) return;                      // don't log the log channel itself
    if (message.author?.id === client.user.id) return;               // skip my own messages
    const logCh = message.guild.channels.cache.get(msgLogId);
    if (!logCh) return;

    const author = message.author;
    const desc =
      `🗑️ **Message deleted** in <#${message.channelId}>\n` +
      (author ? `**Author:** <@${author.id}> · \`${author.tag}\` · \`${author.id}\`\n` : `**Author:** _uncached_\n`) +
      (message.content ? `**Content:**\n${message.content.slice(0, 1800)}`
        : (message.partial ? "_content not cached (sent before restart)_" : "_no text content_"));

    const e = new EmbedBuilder().setColor(COLORS.muted).setDescription(desc).setTimestamp();
    if (author) e.setAuthor({ name: author.tag, iconURL: author.displayAvatarURL?.() });

    // Re-upload attachments so images survive Discord's CDN expiry.
    const files = []; const lines = []; let firstImage = null; let idx = 0;
    if (message.attachments?.size) {
      for (const att of message.attachments.values()) {
        const safe = `${idx++}_${(att.name || "file").replace(/[^a-zA-Z0-9._-]/g, "_")}`;
        files.push({ attachment: att.url, name: safe });
        lines.push(`${att.name || safe} · ${Math.round((att.size || 0) / 1024)} KB`);
        if (!firstImage && ((att.contentType || "").startsWith("image/") || /\.(png|jpe?g|gif|webp)$/i.test(att.name || "")))
          firstImage = safe;
      }
      if (firstImage) e.setImage(`attachment://${firstImage}`);
      e.addFields({ name: `Attachments (${message.attachments.size})`, value: lines.join("\n").slice(0, 1024) });
    }

    await logCh.send({ embeds: [e], files: files.length ? files : undefined })
      .catch(() => logCh.send({ embeds: [e.setImage(null)] }).catch(() => {})); // fallback if URLs already expired
  } catch (err) { console.error("msg-delete log error:", err.message); }
});

client.on(Events.MessageBulkDelete, async (messages, channel) => {
  try {
    if (!channel?.guild) return;
    const msgLogId = gc(channel.guild).msgLogChannelId;
    if (!msgLogId) return;
    if (channel.id === msgLogId) return;
    const logCh = channel.guild.channels.cache.get(msgLogId);
    if (!logCh) return;
    const cached = [...messages.values()].filter(m => m.author);
    const lines = cached.slice(0, 15).map(m => `<@${m.author.id}>: ${(m.content || "[embed/attachment]").slice(0, 80)}`).join("\n");
    const e = new EmbedBuilder().setColor(COLORS.warn).setTitle("🧹 Bulk delete")
      .setDescription(`**${messages.size}** messages deleted in <#${channel.id}>` +
        (lines ? `\n\n${lines}` : "") +
        (cached.length > 15 ? `\n…and ${cached.length - 15} more cached` : "")).setTimestamp();
    logCh.send({ embeds: [e] }).catch(() => {});
  } catch (err) { console.error("bulk-delete log error:", err.message); }
});

client.on(Events.MessageUpdate, async (oldMessage, newMessage) => {
  try {
    if (!newMessage.guild) return;
    const msgLogId = gc(newMessage.guild).msgLogChannelId;
    if (!msgLogId) return;
    if (newMessage.channelId === msgLogId) return;
    if (newMessage.author?.id === client.user.id) return;
    if (oldMessage.content === newMessage.content) return; // ignore embed-resolve / pin / non-content updates
    const logCh = newMessage.guild.channels.cache.get(msgLogId);
    if (!logCh) return;

    const author = newMessage.author;
    const before = oldMessage.partial ? "_not cached (sent before restart)_" : (oldMessage.content || "_empty_");
    const after  = newMessage.content || "_empty_";
    const e = new EmbedBuilder().setColor(COLORS.info)
      .setDescription(
        `✏️ **Message edited** in <#${newMessage.channelId}> · [jump](${newMessage.url})\n` +
        (author ? `**Author:** <@${author.id}> · \`${author.tag}\` · \`${author.id}\`` : ""))
      .addFields(
        { name: "Before", value: before.slice(0, 1024) },
        { name: "After",  value: after.slice(0, 1024) },
      ).setTimestamp();
    if (author) e.setAuthor({ name: author.tag, iconURL: author.displayAvatarURL?.() });
    logCh.send({ embeds: [e] }).catch(() => {});
  } catch (err) { console.error("msg-edit log error:", err.message); }
});

// ── /setup helpers ──────────────────────────────────────────────
function buildSetupEmbed(guild, changes) {
  const g = gc(guild);
  return new EmbedBuilder()
    .setColor(changes.length ? COLORS.success : COLORS.info)
    .setTitle(`🛡️ Guardian setup — ${guild.name}`)
    .setDescription(changes.length
      ? `**Updated:**\n${changes.map(c => `• ${c}`).join("\n")}`
      : "Run `/setup quick` for one-command setup, or `/setup roles` / `/setup channels` / `/setup whitelist` / `/setup failsafe` to configure individual fields. Current settings:")
    .addFields(
      { name: "Mod Role",       value: g.modRoleId ? `<@&${g.modRoleId}>` : "❌ Not set", inline: true },
      { name: "Mute Role",      value: g.muteRoleId ? `<@&${g.muteRoleId}>` : "❌ Not set", inline: true },
      { name: "​",         value: "​", inline: true },
      { name: "Log Channel",    value: g.logChannelId ? `<#${g.logChannelId}>` : "❌ Not set", inline: true },
      { name: "Alert Channel",  value: g.alertChannelId ? `<#${g.alertChannelId}>` : "(uses log)", inline: true },
      { name: "Msg Log",        value: g.msgLogChannelId ? `<#${g.msgLogChannelId}>` : "❌ Not set", inline: true },
      { name: "Whitelist Users",value: g.nukeWhitelistUserIds.length ? g.nukeWhitelistUserIds.map(id => `<@${id}>`).join(", ") : "None", inline: false },
      { name: "Whitelist Roles",value: g.nukeWhitelistRoleIds.length ? g.nukeWhitelistRoleIds.map(id => `<@&${id}>`).join(", ") : "None", inline: false },
      { name: "Failsafe Roles", value: g.failsafeRoleIds.length ? g.failsafeRoleIds.map(id => `<@&${id}>`).join(", ") : "None — configure with `/setup failsafe`", inline: false },
    )
    .setFooter({ text: "Behavioral thresholds are global (.env); these identity settings are per-server." })
    .setTimestamp();
}

// /setup quick — auto-provision a working Muted role + Guardian log category/channels
// for THIS guild only. Reuses existing role/channels matched by name instead of
// duplicating them if run more than once.
async function quickSetupGuild(guild, modRoleOpt) {
  const created = []; const reused = [];

  // 1) Muted role: reuse by name if present, else create with no base permissions.
  let muteRole = guild.roles.cache.find(r => !r.managed && r.name.toLowerCase() === "muted");
  if (muteRole) reused.push(`role <@&${muteRole.id}>`);
  else {
    muteRole = await guild.roles.create({ name: "Muted", color: 0x808080, reason: "Guardian quick setup" }).catch(() => null);
    if (muteRole) created.push(`role <@&${muteRole.id}>`);
  }

  // Deny send/speak on every existing channel so the role actually mutes.
  if (muteRole) {
    for (const ch of guild.channels.cache.values()) {
      if (ch.isThread?.()) continue;
      const opts = {};
      if (ch.isTextBased?.()) Object.assign(opts, {
        SendMessages: false, AddReactions: false,
        CreatePublicThreads: false, CreatePrivateThreads: false, SendMessagesInThreads: false,
      });
      if (ch.isVoiceBased?.()) Object.assign(opts, { Speak: false, Stream: false });
      if (Object.keys(opts).length)
        await ch.permissionOverwrites.edit(muteRole, opts, { reason: "Guardian quick setup: mute role overwrite" }).catch(() => {});
    }
  }

  // 2) "Guardian" category + 3 private log channels: reuse by name if present, else create.
  let category = guild.channels.cache.find(c => c.type === ChannelType.GuildCategory && c.name === "Guardian");
  if (category) reused.push(`category **${category.name}**`);
  else {
    category = await guild.channels.create({ name: "Guardian", type: ChannelType.GuildCategory, reason: "Guardian quick setup" }).catch(() => null);
    if (category) created.push(`category **${category.name}**`);
  }

  const overwrites = [{ id: guild.id, type: 0, deny: [PermissionsBitField.Flags.ViewChannel] }];
  if (modRoleOpt) overwrites.push({ id: modRoleOpt.id, type: 0, allow: [PermissionsBitField.Flags.ViewChannel] });

  async function ensureChannel(name) {
    let ch = guild.channels.cache.find(c =>
      c.type === ChannelType.GuildText && c.name === name && (!category || c.parentId === category.id));
    if (ch) { reused.push(`<#${ch.id}>`); return ch; }
    ch = await guild.channels.create({
      name, type: ChannelType.GuildText, parent: category?.id,
      permissionOverwrites: overwrites, reason: "Guardian quick setup",
    }).catch(() => null);
    if (ch) created.push(`<#${ch.id}>`);
    return ch;
  }

  const logCh    = await ensureChannel("mod-logs");
  const alertCh  = await ensureChannel("mod-alerts");
  const msgLogCh = await ensureChannel("message-logs");

  if (muteRole)   setGuild(guild.id, "muteRoleId", muteRole.id);
  if (logCh)      setGuild(guild.id, "logChannelId", logCh.id);
  if (alertCh)    setGuild(guild.id, "alertChannelId", alertCh.id);
  if (msgLogCh)   setGuild(guild.id, "msgLogChannelId", msgLogCh.id);
  if (modRoleOpt) setGuild(guild.id, "modRoleId", modRoleOpt.id);

  return { created, reused };
}

// ── Slash Command Handler ─────────────────────────────────────
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isChatInputCommand()) return;
  if (!interaction.inGuild())
    return interaction.reply({ content: "❌ This command can only be used in a server.", ephemeral: true });
  const { commandName, guild, member } = interaction;

  try {
  switch (commandName) {

    // ── /mute ──────────────────────────────────────────────
    case "mute": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const target  = interaction.options.getMember("user");
      const minutes = interaction.options.getInteger("minutes") ?? 10;
      const reason  = interaction.options.getString("reason") ?? "No reason provided";
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });
      const muteRoleId = gc(guild).muteRoleId;
      if (!muteRoleId || !guild.roles.cache.get(muteRoleId))
        return interaction.reply({ content: "❌ Mute role not configured. Run `/setup quick` or `/setup roles mute_role:@Role`.", ephemeral: true });

      if (!isWhitelisted(member)) {
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "mute");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("mute", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "mute");
      }
      const ok = await muteUser(target, minutes, reason);
      if (!ok) return interaction.reply({ content: "❌ Mute role not configured. Run `/setup quick` or `/setup roles mute_role:@Role`.", ephemeral: true });
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "mute");
      const stashed = mutedRoles[guild.id]?.[target.id]?.roles?.length ?? 0;
      const e = new EmbedBuilder().setColor(COLORS.muted).setTitle("🔇 Member Muted")
        .setDescription(`<@${target.id}> has been muted for **${minutes > 0 ? minutes + " minutes" : "permanently"}**.\n**Reason:** ${reason}\n📦 **${stashed}** role${stashed === 1 ? "" : "s"} stashed — restored on unmute.`)
        .setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("mute", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /unmute ────────────────────────────────────────────
    case "unmute": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const target = interaction.options.getMember("user");
      if (!target) return interaction.reply({ content: "❌ User not found.", ephemeral: true });
      if (!gc(guild).muteRoleId) return interaction.reply({ content: "❌ Mute role not configured. Run `/setup quick` or `/setup roles mute_role:@Role`.", ephemeral: true });
      const stashed = mutedRoles[guild.id]?.[target.id]?.roles?.length ?? 0;
      await unmuteUser(guild, target.id, `Manual unmute by ${interaction.user.tag}`);
      return interaction.reply({ embeds: [new EmbedBuilder().setColor(COLORS.success).setTitle("🔊 Member Unmuted")
        .setDescription(`<@${target.id}> has been unmuted.\n♻️ Restored **${stashed}** stashed role${stashed === 1 ? "" : "s"}.`).setTimestamp()] });
    }

    // ── /kick ──────────────────────────────────────────────
    case "kick": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const target = interaction.options.getMember("user");
      const reason = interaction.options.getString("reason") ?? "No reason provided";
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });

      if (!isWhitelisted(member)) {
        if (bump(guild.id, member.id, "kicks", config.nukeKickThreshold)) {
          resetBump(guild.id, member.id, "kicks");
          await interaction.reply({ content: "🚨 Anti-nuke triggered.", ephemeral: true });
          return nukeResponse(guild, member, `Issued ${config.nukeKickThreshold}+ kicks via commands in ${config.nukeWindowMs / 1000}s`);
        }
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "kick");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("kick", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "kick");
      }
      await tryDM(target.user, `You were kicked from **${guild.name}**. Reason: ${reason}`);
      await target.kick(reason).catch(() => {});
      secLog(guild, "Member Kicked", `<@${target.id}> kicked by <@${member.id}>: ${reason}`, COLORS.danger);
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "kick");
      const e = new EmbedBuilder().setColor(COLORS.danger).setTitle("👢 Member Kicked")
        .setDescription(`<@${target.id}> has been kicked.\n**Reason:** ${reason}`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("kick", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /ban ───────────────────────────────────────────────
    case "ban": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const target     = interaction.options.getMember("user");
      const reason     = interaction.options.getString("reason") ?? "No reason provided";
      const deleteDays = interaction.options.getInteger("delete_days") ?? 0;
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });

      if (!isWhitelisted(member)) {
        if (bump(guild.id, member.id, "bans", config.nukeBanThreshold)) {
          resetBump(guild.id, member.id, "bans");
          await interaction.reply({ content: "🚨 Anti-nuke triggered.", ephemeral: true });
          return nukeResponse(guild, member, `Issued ${config.nukeBanThreshold}+ bans via commands in ${config.nukeWindowMs / 1000}s`);
        }
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "ban");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("ban", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "ban");
      }
      await tryDM(target.user, `You were banned from **${guild.name}**. Reason: ${reason}`);
      await target.ban({ reason, deleteMessageSeconds: deleteDays * 86400 }).catch(() => {});
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "ban");
      secLog(guild, "Member Banned", `<@${target.id}> banned by <@${member.id}>: ${reason}`, COLORS.danger);
      const e = new EmbedBuilder().setColor(COLORS.danger).setTitle("🔨 Member Banned")
        .setDescription(`<@${target.id}> has been banned.\n**Reason:** ${reason}`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("ban", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /unban ─────────────────────────────────────────────
    case "unban": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const userId = interaction.options.getString("user_id").trim();
      const reason = interaction.options.getString("reason") ?? "No reason provided";
      if (!/^\d{17,20}$/.test(userId)) return interaction.reply({ content: "❌ That doesn't look like a valid user ID.", ephemeral: true });
      const ban = await guild.bans.fetch(userId).catch(() => null);
      if (!ban) return interaction.reply({ content: "❌ That user isn't banned.", ephemeral: true });
      await guild.bans.remove(userId, `Unban by ${interaction.user.tag}: ${reason}`).catch(() => {});
      secLog(guild, "Member Unbanned", `\`${userId}\` unbanned by <@${member.id}>: ${reason}`, COLORS.success);
      return interaction.reply({ embeds: [new EmbedBuilder().setColor(COLORS.success).setTitle("♻️ Member Unbanned")
        .setDescription(`<@${userId}> (\`${userId}\`) has been unbanned.\n**Reason:** ${reason}`).setTimestamp()] });
    }

    // ── /purge ─────────────────────────────────────────────
    case "purge": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const count      = interaction.options.getInteger("count");
      const filterUser = interaction.options.getUser("user");

      if (!isWhitelisted(member)) {
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "purge");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("purge", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "purge");
      }
      await interaction.deferReply({ ephemeral: true });
      let messages = await interaction.channel.messages.fetch({ limit: 100 }).catch(() => null);
      if (!messages) return interaction.editReply("❌ Could not fetch messages.");
      if (filterUser) messages = messages.filter(m => m.author.id === filterUser.id);
      const toDelete = [...messages.values()].slice(0, count);
      const deleted  = await interaction.channel.bulkDelete(toDelete, true).catch(() => null);
      const n = deleted?.size ?? 0;
      secLog(guild, "Purge", `<@${member.id}> purged **${n}** messages in <#${interaction.channelId}>${filterUser ? ` from <@${filterUser.id}>` : ""}`, COLORS.warn);
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "purge");
      const e = new EmbedBuilder().setColor(COLORS.warn).setTitle("🗑️ Purge Complete")
        .setDescription(`Deleted **${n}** messages${filterUser ? ` from <@${filterUser.id}>` : ""}.`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("purge", newUsed, limit) });
      return interaction.editReply({ embeds: [e] });
    }

    // ── /lockdown ──────────────────────────────────────────
    case "lockdown": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const action  = interaction.options.getString("action");
      const channel = interaction.options.getChannel("channel") ?? interaction.channel;
      const lock    = action === "lock";

      if (lock && !isWhitelisted(member)) {
        if (bump(guild.id, member.id, "chLock", config.nukeChannelThreshold)) {
          resetBump(guild.id, member.id, "chLock");
          await interaction.reply({ content: "🚨 Anti-nuke triggered.", ephemeral: true });
          return nukeResponse(guild, member, `Locked ${config.nukeChannelThreshold}+ channels via commands in ${config.nukeWindowMs / 1000}s`);
        }
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "lockdown");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("lockdown", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "lockdown");
      }
      await channel.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: lock ? false : null }).catch(() => {});
      secLog(guild, lock ? "Channel Locked" : "Channel Unlocked",
        `<#${channel.id}> ${lock ? "locked" : "unlocked"} by <@${member.id}>`, lock ? COLORS.danger : COLORS.success);
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "lockdown");
      const e = new EmbedBuilder().setColor(lock ? COLORS.danger : COLORS.success)
        .setTitle(lock ? "🔒 Channel Locked" : "🔓 Channel Unlocked")
        .setDescription(`<#${channel.id}> has been ${lock ? "locked" : "unlocked"}.`).setTimestamp();
      if (lock && !isWhitelisted(member)) e.setFooter({ text: usageFooter("lockdown", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /panic (owner only) — toggles: run again to lift ────
    case "panic": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "❌ Owner only.", ephemeral: true });
      await interaction.deferReply({ ephemeral: true });

      if (isLockdown(guild.id)) {
        let unlocked = 0;
        for (const ch of guild.channels.cache.values()) {
          if (ch.isTextBased() && !ch.isThread()) {
            const ok = await ch.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: null }).then(() => true).catch(() => false);
            if (ok) unlocked++;
          }
        }
        clearLockdown(guild.id);
        alertOwner(guild, `✅ **PANIC LOCKDOWN LIFTED** by <@${member.id}>. Unlocked **${unlocked}** channels.`, COLORS.success, "PANIC LIFTED");
        return interaction.editReply(`✅ Panic lockdown lifted — unlocked **${unlocked}** text channels.`);
      }

      let locked = 0;
      for (const ch of guild.channels.cache.values()) {
        if (ch.isTextBased() && !ch.isThread()) {
          const ok = await ch.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: false }).then(() => true).catch(() => false);
          if (ok) locked++;
        }
      }
      setLockdown(guild.id);
      alertOwner(guild, `🚨 **PANIC LOCKDOWN** engaged by <@${member.id}>. Locked **${locked}** channels. Run \`/panic\` again to lift.`, COLORS.nuke, "PANIC");
      return interaction.editReply(`🚨 Panic lockdown engaged — locked **${locked}** text channels. Run \`/panic\` again to lift.`);
    }

    // ── /warn ──────────────────────────────────────────────
    case "warn": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const target = interaction.options.getMember("user");
      const reason = interaction.options.getString("reason") ?? "No reason provided";
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });

      if (!isWhitelisted(member)) {
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "warn");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("warn", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "warn");
      }
      const total = addWarning(guild.id, target.id, reason, member.id);
      await tryDM(target.user, `You received a warning in **${guild.name}** (#${total}). Reason: ${reason}`);
      secLog(guild, "Warning Issued", `<@${target.id}> warned by <@${member.id}> (total **${total}**): ${reason}`, COLORS.warn);

      // Escalation
      let escalation = "";
      if (config.warnBanAt && total >= config.warnBanAt) {
        await target.ban({ reason: `Auto-escalation: reached ${total} warnings` }).catch(() => {});
        escalation = `\n🔨 **Auto-banned** (reached ${total} warnings).`;
        secLog(guild, "Auto-Escalation", `<@${target.id}> auto-banned at ${total} warnings.`, COLORS.danger);
      } else if (config.warnKickAt && total >= config.warnKickAt) {
        await target.kick(`Auto-escalation: reached ${total} warnings`).catch(() => {});
        escalation = `\n👢 **Auto-kicked** (reached ${total} warnings).`;
        secLog(guild, "Auto-Escalation", `<@${target.id}> auto-kicked at ${total} warnings.`, COLORS.danger);
      } else if (config.warnMuteAt && total >= config.warnMuteAt) {
        await muteUser(target, config.warnMuteMin, `Auto-escalation: reached ${total} warnings`);
        escalation = `\n🔇 **Auto-muted** for ${config.warnMuteMin} min (reached ${total} warnings).`;
      }

      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "warn");
      const e = new EmbedBuilder().setColor(COLORS.warn).setTitle("⚠️ Warning Issued")
        .setDescription(`<@${target.id}> has been warned. **Total: ${total}**\n**Reason:** ${reason}${escalation}`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("warn", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /warnings ──────────────────────────────────────────
    case "warnings": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const target = interaction.options.getUser("user");
      const list = getWarnings(guild.id, target.id);
      if (!list.length) return interaction.reply({ content: `✅ <@${target.id}> has no warnings.`, ephemeral: true });
      const lines = list.slice(-15).map((w, i) =>
        `**${i + 1}.** ${w.reason} — by <@${w.by}> · <t:${Math.floor(w.at / 1000)}:R>`).join("\n");
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.warn)
        .setTitle(`⚠️ Warnings for ${target.tag}`)
        .setDescription(`**Total: ${list.length}**\n\n${lines}`)
        .setFooter({ text: `Escalation: mute@${config.warnMuteAt} · kick@${config.warnKickAt} · ban@${config.warnBanAt}` })
        .setTimestamp()] });
    }

    // ── /clearwarns ────────────────────────────────────────
    case "clearwarns": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const target = interaction.options.getUser("user");
      const had = getWarnings(guild.id, target.id).length;
      clearWarnings(guild.id, target.id);
      secLog(guild, "Warnings Cleared", `<@${member.id}> cleared **${had}** warning(s) for <@${target.id}>`, COLORS.success);
      return interaction.reply({ embeds: [embed(COLORS.success, `Cleared **${had}** warning${had === 1 ? "" : "s"} for <@${target.id}>.`, "Warnings Cleared")], ephemeral: true });
    }

    // ── /limits ────────────────────────────────────────────
    case "limits": {
      if (!isMod(member)) return interaction.reply({ content: "❌ You need the mod role.", ephemeral: true });
      const windowHours = config.modWindowMs / 3600000;
      if (isWhitelisted(member)) {
        return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
          .setTitle("🛡️ Your Mod Limits")
          .setDescription(`✨ **You are whitelisted.** No rate limits apply to your account.`).setTimestamp()] });
      }
      const actions = [
        { key: "ban", emoji: "🔨", label: "Bans" }, { key: "kick", emoji: "👢", label: "Kicks" },
        { key: "mute", emoji: "🔇", label: "Mutes" }, { key: "warn", emoji: "⚠️", label: "Warns" },
        { key: "purge", emoji: "🗑️", label: "Purges" }, { key: "lockdown", emoji: "🔒", label: "Lockdowns" },
      ];
      const fields = actions.map(({ key, emoji, label }) => {
        const { used, limit, remaining } = checkModLimit(guild.id, member.id, key);
        const bar = buildBar(used, limit, 8);
        const pct = Math.round((used / limit) * 100);
        const warn = remaining === 0 ? " 🚫" : remaining <= Math.ceil(limit * 0.2) ? " ⚠️" : "";
        return { name: `${emoji} ${label}${warn}`, value: `\`${bar}\` **${used}/${limit}** used (${pct}%) — **${remaining}** remaining`, inline: false };
      });
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
        .setTitle("📊 Your Mod Action Limits")
        .setDescription(`Rolling **${windowHours}h** window. Limits reset automatically as old actions age out.`)
        .addFields(...fields).setTimestamp()] });
    }

    // ── /antiping ──────────────────────────────────────────
    case "antiping": {
      if (!isOwner(member) && member.id !== guild.ownerId) return interaction.reply({ content: "🔒 Only the bot owner or this server's owner can configure Guardian.", ephemeral: true });
      const sub = interaction.options.getSubcommand();
      const a = ap(guild);
      switch (sub) {
        case "status":
          return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder()
            .setColor(a.enabled ? COLORS.success : COLORS.neutral).setTitle("📡 Anti-Ping — Status")
            .addFields(
              { name: "Enabled", value: a.enabled ? "✅ On" : "⛔ Off", inline: true },
              { name: "Action", value: `\`${a.action}\``, inline: true },
              { name: "Duration", value: `${a.timeoutMin} min`, inline: true },
              { name: "Delete message", value: a.deleteMessage ? "Yes" : "No", inline: true },
              { name: "Ignore replies", value: a.ignoreReplies ? "Yes" : "No", inline: true },
              { name: "Channel notice", value: a.notifyChannel ? "On" : "Off", inline: true },
              { name: "Response", value: `\`\`\`${a.responseTemplate}\`\`\``, inline: false },
              { name: "Protected users", value: a.protectedUsers.length ? a.protectedUsers.map(id => `<@${id}>`).join(", ") : "None", inline: false },
              { name: "Protected roles", value: a.protectedRoles.length ? a.protectedRoles.map(id => `<@&${id}>`).join(", ") : "None", inline: false },
            ).setTimestamp()] });
        case "toggle": {
          const enabled = interaction.options.getBoolean("enabled"); setAntiPing(guild.id, { enabled });
          return interaction.reply({ ephemeral: true, embeds: [embed(enabled ? COLORS.success : COLORS.neutral, `Anti-ping is now **${enabled ? "enabled" : "disabled"}**.`, "Anti-Ping")] });
        }
        case "action": {
          const action = interaction.options.getString("type"); setAntiPing(guild.id, { action });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Punishment set to **${action}**.`, "Anti-Ping")] });
        }
        case "duration": {
          const timeoutMin = interaction.options.getInteger("minutes"); setAntiPing(guild.id, { timeoutMin });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Mute/timeout duration set to **${timeoutMin} min**.`, "Anti-Ping")] });
        }
        case "delete": {
          const deleteMessage = interaction.options.getBoolean("enabled"); setAntiPing(guild.id, { deleteMessage });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Offending messages will ${deleteMessage ? "**be deleted**" : "**not be deleted**"}.`, "Anti-Ping")] });
        }
        case "ignorereplies": {
          const ignoreReplies = interaction.options.getBoolean("enabled"); setAntiPing(guild.id, { ignoreReplies });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Reply-pings will ${ignoreReplies ? "**be ignored**" : "**be punished**"}.`, "Anti-Ping")] });
        }
        case "response": {
          const text = interaction.options.getString("text");
          const responseTemplate = text.toLowerCase() === "default" ? antiPingDefaults.responseTemplate : text;
          setAntiPing(guild.id, { responseTemplate });
          const preview = renderAntiPingResponse({ responseTemplate }, member.id, "@ProtectedUser", `timed out for ${a.timeoutMin} min`);
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info,
            `Response template updated.\n\n**Template:**\n\`\`\`${responseTemplate}\`\`\`\n**Preview:**\n${preview}\n\n_Placeholders: \`{user}\`, \`{targets}\`, \`{action}\`._`, "Anti-Ping")] });
        }
        case "notify": {
          const notifyChannel = interaction.options.getBoolean("enabled"); setAntiPing(guild.id, { notifyChannel });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Public channel warning is now **${notifyChannel ? "on" : "off"}**.`, "Anti-Ping")] });
        }
        case "protect": {
          const action = interaction.options.getString("action");
          const user   = interaction.options.getUser("user");
          let arr = [...a.protectedUsers];
          if (action === "add") {
            if (arr.includes(user.id)) return interaction.reply({ content: `⚠️ <@${user.id}> is already protected.`, ephemeral: true });
            arr.push(user.id);
          } else arr = arr.filter(id => id !== user.id);
          setAntiPing(guild.id, { protectedUsers: arr });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `<@${user.id}> ${action === "add" ? "is now **protected**" : "is **no longer protected**"} from pings.`, "Anti-Ping")] });
        }
        case "protectrole": {
          const action = interaction.options.getString("action");
          const role   = interaction.options.getRole("role");
          let arr = [...a.protectedRoles];
          if (action === "add") {
            if (arr.includes(role.id)) return interaction.reply({ content: `⚠️ <@&${role.id}> is already protected.`, ephemeral: true });
            arr.push(role.id);
          } else arr = arr.filter(id => id !== role.id);
          setAntiPing(guild.id, { protectedRoles: arr });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `<@&${role.id}> ${action === "add" ? "is now **protected**" : "is **no longer protected**"} from pings.`, "Anti-Ping")] });
        }
        case "list":
          return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info).setTitle("📡 Anti-Ping — Protected")
            .addFields(
              { name: "Users", value: a.protectedUsers.length ? a.protectedUsers.map(id => `<@${id}>`).join("\n") : "None", inline: true },
              { name: "Roles", value: a.protectedRoles.length ? a.protectedRoles.map(id => `<@&${id}>`).join("\n") : "None", inline: true },
            ).setTimestamp()] });
      }
      return;
    }

    // ── /setup ─────────────────────────────────────────────
    case "setup": {
      if (!isOwner(member) && member.id !== guild.ownerId) return interaction.reply({ content: "🔒 Only the bot owner or this server's owner can configure Guardian.", ephemeral: true });
      const sub = interaction.options.getSubcommand();

      if (sub === "quick") {
        await interaction.deferReply({ ephemeral: true });
        const modRoleOpt = interaction.options.getRole("mod_role");
        const { created, reused } = await quickSetupGuild(guild, modRoleOpt);
        const e = buildSetupEmbed(guild, []);
        e.setTitle(`🛡️ Guardian quick setup — ${guild.name}`);
        e.setDescription(
          (created.length ? `**Created:** ${created.join(", ")}\n` : "") +
          (reused.length ? `**Reused existing:** ${reused.join(", ")}\n` : "") +
          `\nCurrent settings:`);
        return interaction.editReply({ embeds: [e] });
      }

      if (sub === "view") {
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, [])] });
      }

      if (sub === "roles") {
        const modRole  = interaction.options.getRole("mod_role");
        const muteRole = interaction.options.getRole("mute_role");
        const changes = [];
        if (modRole)  { setGuild(guild.id, "modRoleId",  modRole.id);  changes.push(`Mod role → <@&${modRole.id}>`); }
        if (muteRole) { setGuild(guild.id, "muteRoleId", muteRole.id); changes.push(`Mute role → <@&${muteRole.id}> _(ensure it denies Send Messages)_`); }
        if (!changes.length) return interaction.reply({ content: "❌ Provide at least one role.", ephemeral: true });
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, changes)] });
      }

      if (sub === "channels") {
        const logCh    = interaction.options.getChannel("log_channel");
        const alertCh  = interaction.options.getChannel("alert_channel");
        const msgLogCh = interaction.options.getChannel("msg_log_channel");
        const changes = [];
        if (logCh)    { setGuild(guild.id, "logChannelId",    logCh.id);    changes.push(`Log channel → <#${logCh.id}>`); }
        if (alertCh)  { setGuild(guild.id, "alertChannelId",  alertCh.id);  changes.push(`Alert channel → <#${alertCh.id}>`); }
        if (msgLogCh) { setGuild(guild.id, "msgLogChannelId", msgLogCh.id); changes.push(`Msg log → <#${msgLogCh.id}>`); }
        if (!changes.length) return interaction.reply({ content: "❌ Provide at least one channel.", ephemeral: true });
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, changes)] });
      }

      if (sub === "whitelist") {
        const action = interaction.options.getString("action");
        const user   = interaction.options.getUser("user");
        const role   = interaction.options.getRole("role");
        if (!user && !role) return interaction.reply({ content: "❌ Provide a user or a role.", ephemeral: true });
        const changes = [];
        if (user) {
          let arr = [...gc(guild).nukeWhitelistUserIds];
          if (action === "add" && !arr.includes(user.id)) { arr.push(user.id); changes.push(`Whitelist +user <@${user.id}>`); }
          if (action === "remove") { arr = arr.filter(x => x !== user.id); changes.push(`Whitelist −user <@${user.id}>`); }
          setGuild(guild.id, "nukeWhitelistUserIds", arr);
        }
        if (role) {
          let arr = [...gc(guild).nukeWhitelistRoleIds];
          if (action === "add" && !arr.includes(role.id)) { arr.push(role.id); changes.push(`Whitelist +role <@&${role.id}>`); }
          if (action === "remove") { arr = arr.filter(x => x !== role.id); changes.push(`Whitelist −role <@&${role.id}>`); }
          setGuild(guild.id, "nukeWhitelistRoleIds", arr);
        }
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, changes)] });
      }

      if (sub === "failsafe") {
        const action = interaction.options.getString("action");
        const role   = interaction.options.getRole("role");
        let arr = [...gc(guild).failsafeRoleIds];
        const changes = [];
        if (action === "add" && !arr.includes(role.id)) { arr.push(role.id); changes.push(`Failsafe +role <@&${role.id}>`); }
        if (action === "remove") { arr = arr.filter(x => x !== role.id); changes.push(`Failsafe −role <@&${role.id}>`); }
        setGuild(guild.id, "failsafeRoleIds", arr);
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, changes)] });
      }
      return;
    }

    // ── /config ────────────────────────────────────────────
    case "config": {
      if (!isOwner(member) && member.id !== guild.ownerId) return interaction.reply({ content: "🔒 Only the bot owner or this server's owner can view configuration.", ephemeral: true });
      const windowHours = config.modWindowMs / 3600000;
      const gcfg = gc(guild);
      const acfg = ap(guild);
      const cfgEmbed = new EmbedBuilder().setTitle("🛡️ Guardian Bot — Configuration").setColor(COLORS.info)
        .addFields(
          { name: "🔧 Infrastructure", value: "​", inline: false },
          { name: "Owner",        value: `<@${BOT_OWNER_ID}>`, inline: true },
          { name: "Log Channel",  value: gcfg.logChannelId ? `<#${gcfg.logChannelId}>` : "❌ Not set", inline: true },
          { name: "Alert Channel",value: gcfg.alertChannelId ? `<#${gcfg.alertChannelId}>` : "(uses log)", inline: true },
          { name: "Msg Log",      value: gcfg.msgLogChannelId ? `<#${gcfg.msgLogChannelId}>` : "❌ Not set", inline: true },
          { name: "Mute Role",    value: gcfg.muteRoleId ? `<@&${gcfg.muteRoleId}>` : "❌ Not set", inline: true },
          { name: "Mod Role",     value: gcfg.modRoleId ? `<@&${gcfg.modRoleId}>` : "❌ Not set", inline: true },
          { name: "🏅 Nuke Whitelist Roles", value: gcfg.nukeWhitelistRoleIds.length ? gcfg.nukeWhitelistRoleIds.map(id => `<@&${id}>`).join(", ") : "None", inline: false },
          { name: "🏅 Nuke Whitelist Users", value: gcfg.nukeWhitelistUserIds.length ? gcfg.nukeWhitelistUserIds.map(id => `<@${id}>`).join(", ") : "None", inline: false },
          { name: "💬 Anti-Spam", value: `${config.spamThreshold} msgs / ${config.spamWindowMs}ms · mention≥${config.spamMentionLimit} · dupes≥${config.spamDuplicateLimit} · invites ${config.spamBlockInvites ? "blocked" : "allowed"} → ${config.spamMuteMin} min mute`, inline: false },
          { name: "🚪 Anti-Raid", value: `${config.raidJoinThreshold} joins / ${config.raidWindowMs}ms → ${config.raidLockdownMin} min lockdown · new-acct kick: ${config.raidKickNewOnLock ? `<${config.raidMinAccountAgeMin}m` : "off"}`, inline: false },
          { name: "📡 Anti-Ping", value: `${acfg.enabled ? "On" : "Off"} • \`${acfg.action}\` • ${acfg.timeoutMin} min • ${acfg.protectedUsers.length} users / ${acfg.protectedRoles.length} roles`, inline: false },
          { name: "💣 Anti-Nuke (fast window)", value: `Window: ${config.nukeWindowMs}ms`, inline: false },
          { name: "Chan Del/Create", value: `≥ ${config.nukeChannelThreshold} / ${config.nukeChannelCreateThresh}`, inline: true },
          { name: "Role Del/Create", value: `≥ ${config.nukeRoleThreshold} / ${config.nukeRoleCreateThresh}`, inline: true },
          { name: "Bans / Kicks", value: `≥ ${config.nukeBanThreshold} / ${config.nukeKickThreshold}`, inline: true },
          { name: "Webhooks", value: `≥ ${config.nukeWebhookThreshold}`, inline: true },
          { name: "Dangerous grants", value: `≥ ${config.nukeMemberRoleThreshold}`, inline: true },
          { name: "Bot add", value: `${config.nukeBotAddAction}`, inline: true },
          { name: "⚠️ Warn Escalation", value: `mute @ ${config.warnMuteAt} (${config.warnMuteMin}m) · kick @ ${config.warnKickAt} · ban @ ${config.warnBanAt}`, inline: false },
          { name: `📊 Mod Daily Limits (${windowHours}h — whitelisted exempt)`, value: "​", inline: false },
          { name: "🔨 Bans", value: `${config.modBanLimit}`, inline: true },
          { name: "👢 Kicks", value: `${config.modKickLimit}`, inline: true },
          { name: "🔇 Mutes", value: `${config.modMuteLimit}`, inline: true },
          { name: "⚠️ Warns", value: `${config.modWarnLimit}`, inline: true },
          { name: "🗑️ Purges", value: `${config.modPurgeLimit}`, inline: true },
          { name: "🔒 Lockdowns", value: `${config.modLockdownLimit}`, inline: true },
        )
        .setFooter({ text: "Edit values in .env and restart to apply changes." }).setTimestamp();
      return interaction.reply({ embeds: [cfgEmbed], ephemeral: true });
    }

    // ── /nuketest ──────────────────────────────────────────
    case "nuketest": {
      if (!isOwner(interaction.user) && interaction.user.id !== guild.ownerId)
        return interaction.reply({ content: "❌ Owner only.", ephemeral: true });
      const me = guild.members.me;
      const need = [
        ["View Audit Log", PermissionsBitField.Flags.ViewAuditLog],
        ["Ban Members", PermissionsBitField.Flags.BanMembers],
        ["Kick Members", PermissionsBitField.Flags.KickMembers],
        ["Manage Roles", PermissionsBitField.Flags.ManageRoles],
        ["Manage Channels", PermissionsBitField.Flags.ManageChannels],
        ["Moderate Members", PermissionsBitField.Flags.ModerateMembers],
      ];
      const status = need.map(([n, p]) => `${me?.permissions.has(p) ? "✅" : "❌"} ${n}`).join("\n");
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.success)
        .setTitle("✅ Anti-Nuke Active")
        .setDescription(`The unified audit-log anti-nuke engine is **online**.\n\n**My permissions:**\n${status}`)
        .setTimestamp()] });
    }

    // ── /help ──────────────────────────────────────────────
    case "help": {
      const windowHours = config.modWindowMs / 3600000;
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
        .setTitle("🛡️ Guardian Bot — Commands")
        .addFields(
          { name: "🔇 /mute", value: "`@user [minutes] [reason]` — Mute (roles stashed & restored on unmute)", inline: false },
          { name: "🔊 /unmute", value: "`@user` — Unmute & restore stashed roles", inline: false },
          { name: "👢 /kick", value: "`@user [reason]` — Kick a member", inline: false },
          { name: "🔨 /ban", value: "`@user [reason] [delete_days]` — Ban a member", inline: false },
          { name: "♻️ /unban", value: "`user_id [reason]` — Unban by ID", inline: false },
          { name: "🗑️ /purge", value: "`count [user]` — Bulk-delete messages", inline: false },
          { name: "🔒 /lockdown", value: "`lock|unlock [channel]` — Lock or unlock a channel", inline: false },
          { name: "🚨 /panic", value: "Emergency lock **all** text channels *(owner only)*", inline: false },
          { name: "⚠️ /warn", value: "`@user [reason]` — Warn (auto-escalates to mute/kick/ban)", inline: false },
          { name: "📋 /warnings", value: "`@user` — View a member's warnings", inline: false },
          { name: "🧹 /clearwarns", value: "`@user` — Clear a member's warnings", inline: false },
          { name: "📡 /antiping", value: "Configure ping protection — `status`, `toggle`, `action`, `protect`, etc. *(bot owner only)*", inline: false },
          { name: "📊 /limits", value: "Check your remaining mod action limits today", inline: false },
          { name: "⚙️ /config", value: "View configuration *(bot owner only)*", inline: false },
          { name: "🔧 /setup", value: "`quick` auto-provisions a mute role + log channels in one step; `view`/`roles`/`channels`/`whitelist`/`failsafe` configure individual fields *(bot/server owner only)*", inline: false },
          { name: "🧪 /nuketest", value: "Confirm anti-nuke + check my permissions *(owner only)*", inline: false },
          { name: "⏱️ Rate Limits", value: `Mod actions are rate-limited over a **${windowHours}h** window. Use \`/limits\`.`, inline: false },
        )
        .setFooter({ text: "Guardian Bot v2 • Security Suite" }).setTimestamp()] });
    }
  }
  } catch (err) {
    console.error(`⚠️ command "${interaction.commandName}" failed:`, err);
    const msg = { content: "⚠️ Something went wrong running that command.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});

// ── Boot ──────────────────────────────────────────────────────
client.once(Events.ClientReady, async () => {
  console.log(`✅ Guardian Bot online as ${client.user.tag}`);
  console.log(`👑 Owner: ${BOT_OWNER_ID}`);
  client.user.setActivity("Protecting the server 🛡️", { type: ActivityType.Watching });
  if (!client.shard || client.shard.ids.includes(0)) await registerCommandsGlobal();
  await recoverMutes();

  // Permission self-audit
  for (const guild of client.guilds.cache.values()) {
    const me = guild.members.me;
    if (!me) continue;
    const missing = [];
    if (!me.permissions.has(PermissionsBitField.Flags.ViewAuditLog)) missing.push("View Audit Log (anti-nuke blind without this!)");
    if (!me.permissions.has(PermissionsBitField.Flags.BanMembers))   missing.push("Ban Members");
    if (!me.permissions.has(PermissionsBitField.Flags.ManageRoles))  missing.push("Manage Roles");
    if (!me.permissions.has(PermissionsBitField.Flags.ManageChannels)) missing.push("Manage Channels");
    if (missing.length) console.warn(`⚠️ [${guild.name}] missing permissions: ${missing.join(", ")}`);
  }

  // Take an initial full-guild snapshot, then keep rolling snapshots for nuke recovery.
  for (const guild of client.guilds.cache.values()) {
    try { const r = snapshotGuild(guild); console.log(`📸 [${guild.name}] snapshot: ${r.roles} roles, ${r.channels} channels`); } catch (_) {}
  }
  const snapTimer = setInterval(() => {
    for (const guild of client.guilds.cache.values()) { try { snapshotGuild(guild); } catch (_) {} }
  }, config.snapshotIntervalMs);
  if (snapTimer.unref) snapTimer.unref();
});

client.on("error", e => console.error("client error:", e));
process.on("unhandledRejection", e => console.error("unhandledRejection:", e));

// When added to a new server: snapshot it and notify owner. (Global commands
// already cover new guilds automatically — no per-guild registration needed.)
client.on(Events.GuildCreate, async (guild) => {
  console.log(`➕ Joined guild ${guild.name} (${guild.id})`);
  try { snapshotGuild(guild); } catch (_) {}
  if (config.ownerDM)
    client.users.fetch(BOT_OWNER_ID)
      .then(u => u.send(`➕ Guardian was added to **${guild.name}** (\`${guild.id}\`). Run \`/setup quick\` there to auto-provision a mute role + log channels in one step, then \`/setup roles mod_role:@YourStaffRole\` to finish.`))
      .catch(() => {});
});

// Periodic sweep: trim stale tracker entries + self-defense health check.
const healthState = new Map(); // guildId -> last-known-ok boolean
const sweep = setInterval(() => {
  const now = Date.now();
  for (const [key, arr] of spamTracker) if (!arr.length || now - arr[arr.length - 1] > config.spamWindowMs * 5) spamTracker.delete(key);
  for (const [key, d] of dupeTracker)  if (now - d.ts > config.spamWindowMs * 5) dupeTracker.delete(key);
  for (const [key, e] of nukeTracker) {
    let any = false;
    for (const k in e) { e[k] = pruneOld(e[k]); if (e[k].length) any = true; }
    if (!any) nukeTracker.delete(key);
  }
  for (const [gid, arr] of joinTracker) {
    const pruned = arr.filter(t => now - t < config.raidWindowMs);
    if (pruned.length) joinTracker.set(gid, pruned); else joinTracker.delete(gid);
  }
  for (const [gid, arr] of nukeStormTracker) {
    const pruned = arr.filter(t => now - t < config.nukeStormWindowMs);
    if (pruned.length) nukeStormTracker.set(gid, pruned); else nukeStormTracker.delete(gid);
  }
  // Self-defense: if I lose the permissions anti-nuke needs, alert the owner (once per state change).
  for (const guild of client.guilds.cache.values()) {
    const me = guild.members.me;
    if (!me) continue;
    const ok = me.permissions.has(PermissionsBitField.Flags.ViewAuditLog) &&
               me.permissions.has(PermissionsBitField.Flags.BanMembers) &&
               me.permissions.has(PermissionsBitField.Flags.ManageRoles);
    if (healthState.get(guild.id) !== false && !ok)
      alertOwner(guild, "⚠️ I appear to have **lost critical permissions** (View Audit Log / Ban / Manage Roles). Anti-nuke may be blind — check my role position and permissions immediately.", COLORS.danger, "SELF-DEFENSE");
    healthState.set(guild.id, ok);
  }
}, 60000);
if (sweep.unref) sweep.unref();

// Graceful shutdown: flush the DB (WAL) and disconnect cleanly.
for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => {
    console.log(`\n${sig} received — shutting down…`);
    try { db.close(); } catch (_) {}
    try { client.destroy(); } catch (_) {}
    process.exit(0);
  });
}

client.login(TOKEN);
