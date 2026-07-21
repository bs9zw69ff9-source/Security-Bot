// ============================================================
//  GUARDIAN BOT - Discord Security Bot (multi-server)
//  v3 - SQLite persistence, global commands, shard-ready
//  Required: npm install discord.js dotenv better-sqlite3
//  Optional (scale >2500 servers): run `node shard.js` instead of `node index.js`
// ============================================================

require("dotenv").config();
const fs   = require("fs");
const path = require("path");
const {
  Client, GatewayIntentBits, Partials, EmbedBuilder,
  PermissionsBitField, AuditLogEvent, Events,
  REST, Routes, SlashCommandBuilder,
  ActivityType, ChannelType,
  ButtonBuilder, ButtonStyle, ActionRowBuilder,
  ModalBuilder, TextInputBuilder, TextInputStyle,
} = require("discord.js");

const TOKEN     = process.env.DISCORD_TOKEN;
const CLIENT_ID = process.env.CLIENT_ID;
const GUILD_ID  = process.env.GUILD_ID;

// ── OWNER(S) ───────────────────────────────────────────────────
// Always fully trusted: immune to anti-nuke, rate limits, and all guards.
// Also unlocks the hidden, non-slash owner commands (!failsafe, !restore, etc).
// BOT_OWNER_IDS (comma-separated) and/or BOT_OWNER_ID (singular, kept for
// backward compatibility) are merged into one trusted set. Falls back to the
// original hardcoded default if neither env var is set, so this still runs
// out of the box - override it for any real deployment.
const configuredOwnerIds = [
  ...(process.env.BOT_OWNER_IDS || "").split(",").map(s => s.trim()).filter(Boolean),
  ...(process.env.BOT_OWNER_ID ? [process.env.BOT_OWNER_ID.trim()] : []),
];
const BOT_OWNER_IDS = new Set(configuredOwnerIds.length ? configuredOwnerIds : ["1014251293159731310"]);

const ANTIPING_FILE = path.join(__dirname, "antiping.json");
const MUTED_FILE    = path.join(__dirname, "mutedroles.json");
const WARN_FILE     = path.join(__dirname, "warnings.json");

// ── Database (SQLite via better-sqlite3) ──────────────────────
// Write-through persistence: fast in-memory maps stay the source of truth for
// reads; every change is mirrored to a single ACID-safe .db file. This replaces
// the old JSON files (which corrupt under concurrent writes and don't scale).
// Requires: npm install better-sqlite3
const Database = require("better-sqlite3");
const DB_FILE = process.env.GUARDIAN_DB_FILE || path.join(__dirname, "guardian.db");
const db = new Database(DB_FILE);
db.pragma("journal_mode = WAL");
db.pragma("busy_timeout = 5000");
for (const t of ["guild_settings", "antiping", "warnings", "muted_roles", "snapshots", "failsafe", "mod_rates", "lockdown_state", "tickets", "ticket_channels", "applications"])
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

  // ── Mod rate limits (24-hour rolling window, mods only - whitelisted users exempt) ──
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

// Local forensic trail - appended for every security event; survives a wiped log channel.
const FORENSIC_FILE = path.join(__dirname, "security_log.jsonl");
function appendForensic(guildId, kind, data) {
  try { fs.appendFileSync(FORENSIC_FILE, JSON.stringify({ t: new Date().toISOString(), guildId, kind, ...data }) + "\n"); }
  catch (_) {}
}

// ── State ─────────────────────────────────────────────────────
// Every tracker below is keyed (directly or via a "guildId:userId" composite
// key) by guild, so activity in one server can never trip detection or limits
// in another - required for correct multi-server operation.
//
// spamTracker/dupeTracker/joinTracker/nukeTracker/nukeStormTracker are
// deliberately in-memory only: their windows are seconds, so a restart
// naturally (and safely) interrupting a fast burst is fine, and persisting
// sub-minute counters isn't worth the write overhead. modRateTracker
// (24h limits) and lockdown state are NOT - losing those on restart would
// silently reset a mod's daily limits or drop a guild out of an active
// raid/panic lockdown, so both are persisted to SQLite (write-through, same
// pattern as guild settings / warnings / muted roles).
const spamTracker = new Map();     // "gid:uid" -> [timestamps]
const dupeTracker = new Map();     // "gid:uid" -> { content, count, ts }
const joinTracker = new Map();     // gid -> [timestamps]
const nukeTracker = new Map();     // "gid:uid" -> dynamic action arrays

// ── Mod rate limits (persisted to SQLite `mod_rates`) ──────────
let modRates = {}; // { [guildId]: { [userId]: { bans:[], kicks:[], mutes:[], purges:[], lockdowns:[], warns:[] } } }
function loadModRates() { modRates = dbLoadAll("mod_rates"); }
function saveModRates(gid) { dbPut("mod_rates", gid, modRates[gid]); }
loadModRates();

// ── Lockdown state (persisted to SQLite `lockdown_state`) ──────
// { [guildId]: { reason: "raid"|"panic"|"nukestorm", lockedAt, expiresAt: number|null } }
let lockdownState = {};
function loadLockdownState() { lockdownState = dbLoadAll("lockdown_state"); }
function saveLockdownState(gid) { dbPut("lockdown_state", gid, lockdownState[gid]); }
loadLockdownState();
const isLockdown = (gid) => !!lockdownState[gid];
function setLockdown(gid, reason = "manual", expiresAt = null) {
  lockdownState[gid] = { reason, lockedAt: Date.now(), expiresAt };
  saveLockdownState(gid);
}
function clearLockdown(gid) {
  delete lockdownState[gid];
  saveLockdownState(gid);
}
// Reopen every text channel and clear lockdown state for a guild (shared by
// the raid auto-lift timer, /panic unlock, and boot recovery of an expired lock).
async function liftLockdownChannels(guild, note) {
  guild.channels.cache.forEach(ch => {
    if (ch.isTextBased() && !ch.isThread()) ch.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: null }).catch(() => {});
  });
  clearLockdown(guild.id);
  secLog(guild, "Lockdown Lifted", note);
}

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

// Effective per-guild config - STRICTLY per server (no global fallback, so one
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
// the HOME guild (GUILD_ID) ONLY - never applied globally, so other servers stay clean.
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

// ── Ticket system config (persisted to SQLite `tickets`) ──────
// { [guildId]: { panelChannelId, panelMessageId, categoryId, types: [{key,label,emoji,logChannelId}] } }
let ticketConfigs = {};
function loadTicketConfigs() { ticketConfigs = dbLoadAll("tickets"); }
function saveTicketConfig(gid) { dbPut("tickets", gid, ticketConfigs[gid]); }
loadTicketConfigs();
function getTicketConfig(guildId) {
  const c = ticketConfigs[guildId] || {};
  return {
    panelChannelId: c.panelChannelId || "",
    panelMessageId: c.panelMessageId || "",
    categoryId: c.categoryId || "",
    types: Array.isArray(c.types) ? c.types : [],
  };
}
function setTicketConfig(guildId, patch) {
  ticketConfigs[guildId] = { ...getTicketConfig(guildId), ...patch };
  saveTicketConfig(guildId);
}

// ── Open ticket tracking (persisted to SQLite `ticket_channels`) ──
// { [guildId]: { [channelId]: { typeKey, openerId, openedAt, claimedBy, reason } } }
let ticketChannels = {};
function loadTicketChannels() { ticketChannels = dbLoadAll("ticket_channels"); }
function saveTicketChannelsFor(gid) { dbPut("ticket_channels", gid, ticketChannels[gid]); }
loadTicketChannels();
function getOpenTicket(guildId, channelId) { return ticketChannels[guildId]?.[channelId] || null; }
function setOpenTicket(guildId, channelId, data) {
  if (!ticketChannels[guildId]) ticketChannels[guildId] = {};
  ticketChannels[guildId][channelId] = data;
  saveTicketChannelsFor(guildId);
}
function deleteOpenTicket(guildId, channelId) {
  if (!ticketChannels[guildId]) return;
  delete ticketChannels[guildId][channelId];
  if (!Object.keys(ticketChannels[guildId]).length) delete ticketChannels[guildId];
  saveTicketChannelsFor(guildId);
}
function findOpenTicketByUser(guildId, userId, typeKey) {
  const chans = ticketChannels[guildId] || {};
  for (const [chId, t] of Object.entries(chans)) {
    if (t.openerId === userId && t.typeKey === typeKey) return chId;
  }
  return null;
}

// One-time seed: pre-configure the exact ticket types + panel channel requested
// for the HOME guild (GUILD_ID) only, if nothing's configured yet. Never
// overwrites an existing configuration, and never applies to any other guild -
// use `/tickets addtype` / `/tickets panel` for any other server.
function migrateTicketsToHomeGuild() {
  if (!GUILD_ID) return;
  if (getTicketConfig(GUILD_ID).types.length) return;
  setTicketConfig(GUILD_ID, {
    panelChannelId: "1528754448002711592",
    types: [
      { key: "report_player",   label: "Report Player",   emoji: "🚨", logChannelId: "1528754493536342127" },
      { key: "general_support", label: "General Support", emoji: "🎫", logChannelId: "1528754490902053034" },
      { key: "ban_appeal",      label: "Ban Appeals",     emoji: "⚖️", logChannelId: "1528754492147896500" },
      { key: "staff_report",    label: "Staff Reports",   emoji: "🛡️", logChannelId: "1528754494958080080" },
      { key: "police_report",   label: "Police Reports",  emoji: "👮", logChannelId: "1528754496392527962" },
    ],
  });
  console.log(`🎫 Seeded default ticket types + panel channel for home guild (${GUILD_ID})`);
}
migrateTicketsToHomeGuild();

// ── Application system config (persisted to SQLite `applications`) ──
// { [guildId]: { apps: { [key]: { key,label,emoji,panelChannelId,panelMessageId,reviewChannelId,acceptedRoleIds:[],questions:[] } } } }
let applicationConfigs = {};
function loadApplicationConfigs() { applicationConfigs = dbLoadAll("applications"); }
function saveApplicationConfig(gid) { dbPut("applications", gid, applicationConfigs[gid]); }
loadApplicationConfigs();
function getApplications(guildId) {
  const c = applicationConfigs[guildId];
  return (c && c.apps && typeof c.apps === "object") ? c.apps : {};
}
function getApplication(guildId, key) {
  return getApplications(guildId)[key] || null;
}
function setApplication(guildId, key, patch) {
  if (!applicationConfigs[guildId]) applicationConfigs[guildId] = { apps: {} };
  if (!applicationConfigs[guildId].apps) applicationConfigs[guildId].apps = {};
  const prev = applicationConfigs[guildId].apps[key] || {};
  applicationConfigs[guildId].apps[key] = { ...prev, ...patch };
  saveApplicationConfig(guildId);
}

// One-time seed: pre-configure the exact application types + panel/review
// channels + accepted roles requested for the HOME guild (GUILD_ID) only, if
// nothing's configured yet. Never overwrites an existing configuration, and
// never applies to any other guild - use `/applications` for other servers.
function migrateApplicationsToHomeGuild() {
  if (!GUILD_ID) return;
  if (Object.keys(getApplications(GUILD_ID)).length) return;
  const FAMILY_Q = (fam) => ([
    "How old are you?",
    "Whats your discord and ingame name",
    `Why do you want to join ${fam}?`,
    "How will you help?",
  ]);
  const apps = {
    gambino: {
      key: "gambino", label: "Gambino", emoji: "💼",
      panelChannelId: "1528798524660252814", panelMessageId: "",
      reviewChannelId: "1529100361720266803",
      acceptedRoleIds: ["1528801101003096295", "1528801216518426866", "1528802048131338330"],
      questions: FAMILY_Q("Gambino"), minAge: 14, minMemberTime: "3 days",
    },
    colombo: {
      key: "colombo", label: "Colombo", emoji: "🕴️",
      panelChannelId: "1528798524660252814", panelMessageId: "",
      reviewChannelId: "1528805634995261520",
      acceptedRoleIds: ["1528801101003096295", "1528802048131338330", "1528801296411394148"],
      questions: FAMILY_Q("Colombo"), minAge: 14, minMemberTime: "3 days",
    },
    staff: {
      key: "staff", label: "Staff", emoji: "🛡️",
      panelChannelId: "1528754443129196747", panelMessageId: "",
      reviewChannelId: "1528754486678392875",
      acceptedRoleIds: ["1528754350963556466"],
      questions: [
        "DOB",
        "IGN",
        "Do you have any previous experience and how did you learn from that",
        "Why do you wish to join",
        "How will you make a meaningful impact to the community",
        "How are you better than other applicants",
      ],
      minAge: 15, minMemberTime: "2 weeks",
    },
    nypd: {
      key: "nypd", label: "NYPD", emoji: "👮",
      panelChannelId: "1528754445968740472", panelMessageId: "",
      reviewChannelId: "1528754488339464192",
      acceptedRoleIds: ["1528754363726827572", "1528754358697853050", "1528754369019777034"],
      questions: [
        "How old are you?",
        "Whats your discord and ingame name",
        "Why do you want to join the NYPD?",
        "How will you help?",
        "What would you do if someone is robbing a gun store?",
        "A higher up is giving an unlawful order, what will you do?",
      ],
      minAge: 14, minMemberTime: "1 week",
    },
  };
  applicationConfigs[GUILD_ID] = { apps, reqDefaultsV1: true };
  saveApplicationConfig(GUILD_ID);
  console.log(`📝 Seeded default application types (gambino, colombo, staff, nypd) for home guild (${GUILD_ID})`);
}
migrateApplicationsToHomeGuild();

// Backfill the per-application age / member-time requirements onto the home
// guild's already-seeded apps (added after the initial seed). Runs once,
// guarded by reqDefaultsV1, so it never clobbers later manual edits.
function migrateApplicationRequirements() {
  if (!GUILD_ID) return;
  const cfg = applicationConfigs[GUILD_ID];
  if (!cfg || !cfg.apps || cfg.reqDefaultsV1) return;
  const desired = {
    gambino: { minAge: 14, minMemberTime: "3 days" },
    colombo: { minAge: 14, minMemberTime: "3 days" },
    staff:   { minAge: 15, minMemberTime: "2 weeks" },
    nypd:    { minAge: 14, minMemberTime: "1 week" },
  };
  for (const [key, req] of Object.entries(desired)) if (cfg.apps[key]) Object.assign(cfg.apps[key], req);
  cfg.reqDefaultsV1 = true;
  saveApplicationConfig(GUILD_ID);
  console.log(`📝 Applied per-application requirements (staff 15/2wk, family 14/3d, nypd 14/1wk) for home guild (${GUILD_ID})`);
}
migrateApplicationRequirements();

// Backfill the new staff application questions onto the home guild's
// already-seeded staff app. Runs once, guarded by staffQuestionsV2, so it
// never clobbers a later manual edit via /applications setquestions.
function migrateStaffQuestionsV2() {
  if (!GUILD_ID) return;
  const cfg = applicationConfigs[GUILD_ID];
  if (!cfg || !cfg.apps || cfg.staffQuestionsV2) return;
  if (cfg.apps.staff) cfg.apps.staff.questions = [
    "DOB",
    "IGN",
    "Do you have any previous experience and how did you learn from that",
    "Why do you wish to join",
    "How will you make a meaningful impact to the community",
    "How are you better than other applicants",
  ];
  cfg.staffQuestionsV2 = true;
  saveApplicationConfig(GUILD_ID);
  console.log(`📝 Applied updated staff application questions for home guild (${GUILD_ID})`);
}
migrateStaffQuestionsV2();

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
// Target roles are configured per guild via `/setup failsafe` (gc(guild).failsafeRoleIds) -
// NOT hardcoded, so this works for whatever server the bot is running in, not just one.
const FAILSAFE_FILE = path.join(__dirname, "failsafe_backup.json");

let failsafeBackup = {}; // { [guildId]: { savedAt, roles: [ {…role props, position, members[]} ] } }
function loadFailsafe() { importJsonIfPresent("failsafe", FAILSAFE_FILE); failsafeBackup = dbLoadAll("failsafe"); }
function saveFailsafe(gid) { dbPut("failsafe", gid, failsafeBackup[gid]); }
loadFailsafe();

// !failsafe - back up the target roles, delete them, and kick every bot.
async function runFailsafe(message) {
  const guild = message.guild;
  const failsafeRoleIds = gc(guild).failsafeRoleIds;
  if (!failsafeRoleIds.length)
    return message.reply("There are no failsafe roles set up for this server yet. Add some with `/setup failsafe action:add role:@Role` first.").catch(() => {});

  await message.reply("🛡️ **FAILSAFE engaged** - backing up, then purging roles & bots…").catch(() => {});
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
    `• Roles deleted: **${deleted}**` + (failedRoles.length ? ` - failed: ${failedRoles.join(", ")}` : "") + `\n` +
    `• Bots kicked: **${kicked}**` + (failedBots.length ? ` - failed: ${failedBots.join(", ")}` : "") + `\n` +
    `Run \`!restore\` to rebuild the roles.`;
  await message.reply(report).catch(() => {});
  alertOwner(guild, report, COLORS.nuke, "FAILSAFE");
}

// !restore - recreate the backed-up roles exactly, in the same position, with members.
async function runRestore(message) {
  const guild = message.guild;
  const backup = failsafeBackup[guild.id];
  if (!backup || !backup.roles?.length)
    return message.reply("I do not have a failsafe backup saved for this server.").catch(() => {});

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
        .edit(role, opts, { reason: "Failsafe restore: channel access" })
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
    `• Roles recreated: **${created.length}/${backup.roles.length}**` + (failed.length ? ` - failed: ${failed.join(", ")}` : "") + `\n` +
    `• Channel overwrites restored: **${owRestored}**\n` +
    `• Member assignments restored: **${reassigned}**\n` +
    `_Note: recreated roles get new IDs (Discord assigns them) - names, colors, permissions, positions, channel access, and members are preserved._`;
  await message.reply(report).catch(() => {});
  alertOwner(guild, report, COLORS.success, "FAILSAFE RESTORE");
}

// ── Full-guild snapshot + rollback (survive & undo a nuke) ────
const SNAPSHOT_FILE = path.join(__dirname, "guild_snapshot.json");
let snapshots = {}; // { [guildId]: [ { takenAt, name, roles[], channels[] } ] }  (newest last)
function loadSnapshots() { importJsonIfPresent("snapshots", SNAPSHOT_FILE); snapshots = dbLoadAll("snapshots"); }
function saveSnapshots(gid) { dbPut("snapshots", gid, snapshots[gid]); }
loadSnapshots();

async function snapshotGuild(guild) {
  // Full member cache is required to capture accurate role membership (large
  // guilds don't get a complete member list from the gateway by default).
  await guild.members.fetch().catch(() => {});
  const roles = [...guild.roles.cache.values()]
    .filter(r => r.id !== guild.id && !r.managed)
    .sort((a, b) => a.position - b.position)
    .map(r => ({
      id: r.id, name: r.name, color: r.color, hoist: r.hoist, mentionable: r.mentionable,
      permissions: r.permissions.bitfield.toString(), position: r.position,
      members: r.members.map(m => m.id),
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

// Restore the guild to look EXACTLY like the latest snapshot: deletes anything
// not in the snapshot (roles/channels), corrects anything that drifted
// (permissions, overwrites, channel settings), re-syncs role membership to
// match exactly (adds AND removes), and recreates anything missing.
// Destructive by design - requires a ✅ confirmation before touching anything.
async function rollbackGuild(guild, message) {
  const snap = (snapshots[guild.id] || []).slice(-1)[0];
  if (!snap) { message?.reply("There is no snapshot saved yet. Take one with `!snapshot` first."); return; }

  await guild.roles.fetch().catch(() => {});
  await guild.channels.fetch().catch(() => {});
  await guild.members.fetch().catch(() => {});

  const snapRoleNames = new Set(snap.roles.map(r => r.name));
  const extraRoles = guild.roles.cache.filter(r =>
    r.id !== guild.id && !r.managed && !snapRoleNames.has(r.name));
  const snapChanKeys = new Set(snap.channels.map(c => `${c.name}::${c.type}`));
  const extraChannels = guild.channels.cache.filter(c =>
    !(c.isThread && c.isThread()) && !snapChanKeys.has(`${c.name}::${c.type}`));

  if (message) {
    const warning = await message.reply(
      `⚠️ **Full rollback to the snapshot from <t:${Math.floor(snap.takenAt / 1000)}:R>.** This will:\n` +
      `• **Delete ${extraRoles.size}** role(s) not in that snapshot\n` +
      `• **Delete ${extraChannels.size}** channel(s) not in that snapshot\n` +
      `• Correct permissions/overwrites on everything else to match exactly\n` +
      `• Re-sync role membership to match the snapshot (adds **and** removes members)\n\n` +
      `Anything created since the snapshot was taken - legitimate or not - will be deleted. ` +
      `React with ✅ within 30s to confirm, or ignore to cancel.`
    ).catch(() => null);
    if (!warning) return;
    await warning.react("✅").catch(() => {});
    const collected = await warning.awaitReactions({
      filter: (reaction, user) => reaction.emoji.name === "✅" && user.id === message.author.id,
      max: 1, time: 30000,
    }).catch(() => null);
    if (!collected || !collected.size) {
      await message.reply("Rollback cancelled - I did not get a confirmation in time.").catch(() => {});
      return;
    }
  }

  message?.reply(`♻️ **Rolling back** to snapshot from <t:${Math.floor(snap.takenAt / 1000)}:R> - deleting extras, correcting drift, recreating missing…`).catch(() => {});

  // 1) Delete anything not in the snapshot. Channels before categories so an
  //    emptied category isn't left behind pointlessly (not required, just tidy).
  let rolesDeleted = 0;
  for (const role of extraRoles.values()) {
    if (!role.editable) continue;
    const ok = await role.delete("Rollback: not in snapshot").then(() => true).catch(() => false);
    if (ok) rolesDeleted++;
  }
  let chansDeleted = 0;
  const extraOrdered = [
    ...extraChannels.filter(c => c.type !== ChannelType.GuildCategory).values(),
    ...extraChannels.filter(c => c.type === ChannelType.GuildCategory).values(),
  ];
  for (const ch of extraOrdered) {
    const ok = await ch.delete("Rollback: not in snapshot").then(() => true).catch(() => false);
    if (ok) chansDeleted++;
  }

  // 2) Roles: correct existing ones (matched by name) to match exactly; create missing ones.
  const roleMap = {}; let rolesCreated = 0, rolesCorrected = 0;
  for (const sr of [...snap.roles].sort((a, b) => a.position - b.position)) {
    let live = guild.roles.cache.find(r => r.name === sr.name && !r.managed && r.id !== guild.id);
    const props = { name: sr.name, color: sr.color, hoist: sr.hoist, mentionable: sr.mentionable, permissions: BigInt(sr.permissions) };
    if (live) {
      const ok = await live.edit({ ...props, reason: "Rollback: correct drifted role" }).then(() => true).catch(() => false);
      if (ok) rolesCorrected++;
    } else {
      live = await guild.roles.create({ ...props, reason: "Rollback: recreate role" }).catch(() => null);
      if (live) rolesCreated++;
    }
    if (live) roleMap[sr.id] = live;
  }
  const rolePos = Object.entries(roleMap).map(([oldId, role]) => ({
    role: role.id, position: snap.roles.find(r => r.id === oldId)?.position || 1,
  }));
  if (rolePos.length) await guild.roles.setPositions(rolePos).catch(() => {});

  // 2b) Re-sync role membership exactly to the snapshot: add whoever's missing,
  //     remove whoever has the role now but isn't in the snapshot's member list.
  let membersAdded = 0, membersRemoved = 0;
  for (const sr of snap.roles) {
    const live = roleMap[sr.id];
    if (!live) continue;
    const wanted = new Set(sr.members || []);
    for (const uid of wanted) {
      if (live.members.has(uid)) continue;
      const m = await guild.members.fetch(uid).catch(() => null);
      if (!m) continue;
      const ok = await m.roles.add(live, "Rollback: restore role membership").then(() => true).catch(() => false);
      if (ok) membersAdded++;
    }
    for (const m of live.members.values()) {
      if (wanted.has(m.id)) continue;
      const ok = await m.roles.remove(live, "Rollback: role membership not in snapshot").then(() => true).catch(() => false);
      if (ok) membersRemoved++;
    }
  }

  // Remap overwrite targets: @everyone (guild.id) is stable, roles remap by name, members stay.
  const remapOw = (ows) => {
    const out = [];
    for (const o of ows) {
      let id = o.id;
      if (o.type === 0) { // role overwrite
        if (id === guild.id) { /* @everyone - id stable */ }
        else if (roleMap[id]) id = roleMap[id].id;
        else continue; // references a role that no longer exists and wasn't recreated
      }
      out.push({ id, type: o.type, allow: BigInt(o.allow), deny: BigInt(o.deny) });
    }
    return out;
  };

  // 3) Channels: correct existing ones (incl. overwrites) to match exactly; create missing ones.
  //    Categories first so children can attach to a freshly created one.
  const chanMap = {}; let chansCreated = 0, chansCorrected = 0;
  const cats = snap.channels.filter(c => c.type === ChannelType.GuildCategory);
  const rest = snap.channels.filter(c => c.type !== ChannelType.GuildCategory);
  for (const c of cats) {
    let live = guild.channels.cache.find(ch => ch.type === ChannelType.GuildCategory && ch.name === c.name);
    const overwrites = remapOw(c.overwrites);
    if (live) {
      await live.permissionOverwrites.set(overwrites, "Rollback: correct category overwrites").catch(() => {});
      chansCorrected++;
    } else {
      live = await guild.channels.create({ name: c.name, type: ChannelType.GuildCategory, permissionOverwrites: overwrites, reason: "Rollback" }).catch(() => null);
      if (live) chansCreated++;
    }
    if (live) chanMap[c.id] = live;
  }
  for (const c of rest) {
    let live = guild.channels.cache.find(ch => ch.name === c.name && ch.type === c.type);
    const overwrites = remapOw(c.overwrites);
    const opts = { name: c.name, type: c.type, reason: "Rollback" };
    if (c.parentId && chanMap[c.parentId]) { opts.parent = chanMap[c.parentId].id; opts.lockPermissions = false; }
    if (c.type === ChannelType.GuildText || c.type === ChannelType.GuildAnnouncement) {
      opts.topic = c.topic || null;
      opts.nsfw = !!c.nsfw;
      opts.rateLimitPerUser = c.rateLimit || 0;
    }
    if (c.type === ChannelType.GuildVoice || c.type === ChannelType.GuildStageVoice) {
      if (c.bitrate) opts.bitrate = c.bitrate;
      opts.userLimit = c.userLimit || 0;
    }
    if (live) {
      await live.edit(opts).catch(() => {});
      await live.permissionOverwrites.set(overwrites, "Rollback: correct channel overwrites").catch(() => {});
      chansCorrected++;
    } else {
      live = await guild.channels.create({ ...opts, permissionOverwrites: overwrites }).catch(() => null);
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
    `♻️ **Full rollback complete.**\n` +
    `• Roles: **${rolesCreated}** created, **${rolesCorrected}** corrected, **${rolesDeleted}** deleted (not in snapshot)\n` +
    `• Channels: **${chansCreated}** created, **${chansCorrected}** corrected, **${chansDeleted}** deleted (not in snapshot)\n` +
    `• Role membership: **${membersAdded}** added, **${membersRemoved}** removed to match the snapshot\n` +
    `_Recreated items get new Discord-assigned IDs; matched-by-name items were corrected in place._`;
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
    `This is getting serious - ${reason}. I'm putting the whole server into emergency lockdown: pulling dangerous roles from everyone who isn't whitelisted and locking every channel.`,
    COLORS.nuke, "Emergency Lockdown");
  await guild.members.fetch().catch(() => {});
  for (const m of guild.members.cache.values()) {
    if (m.user.bot || isWhitelisted(m)) continue;
    const danger = m.roles.cache.filter(r => r.permissions.any(DANGER_PERMS) && r.editable);
    if (danger.size) m.roles.remove([...danger.keys()], "Nuke-storm lockdown").catch(() => {});
  }
  for (const ch of guild.channels.cache.values()) {
    if (ch.isTextBased() && !ch.isThread()) ch.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: false }).catch(() => {});
  }
  setLockdown(guild.id, "nukestorm", null);
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
    GatewayIntentBits.DirectMessages, // needed to receive applicants' DM answers
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
    .setName("status").setDescription("Bot health: uptime, latency, guild count, memory (bot owner only)"),

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
    .addSubcommand(s => s.setName("response").setDescription("Customize the warning message - {user} {targets} {action}")
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

  new SlashCommandBuilder()
    .setName("tickets").setDescription("Configure the ticket system")
    .addSubcommand(s => s.setName("addtype").setDescription("Add or update a ticket type")
      .addStringOption(o => o.setName("key").setDescription("Short internal id, e.g. report_player").setRequired(true))
      .addStringOption(o => o.setName("label").setDescription("Button label shown to users").setRequired(true))
      .addStringOption(o => o.setName("emoji").setDescription("Emoji for the button (e.g. 🚨)").setRequired(true))
      .addChannelOption(o => o.setName("log_channel").setDescription("Where this type's logs + transcripts go").setRequired(true).addChannelTypes(ChannelType.GuildText)))
    .addSubcommand(s => s.setName("removetype").setDescription("Remove a ticket type")
      .addStringOption(o => o.setName("key").setDescription("The type's key").setRequired(true)))
    .addSubcommand(s => s.setName("listtypes").setDescription("List configured ticket types"))
    .addSubcommand(s => s.setName("category").setDescription("Set the category new ticket channels are created under")
      .addChannelOption(o => o.setName("category").setDescription("Category channel").setRequired(true).addChannelTypes(ChannelType.GuildCategory)))
    .addSubcommand(s => s.setName("panel").setDescription("Post or refresh the ticket panel")
      .addChannelOption(o => o.setName("channel").setDescription("Channel to post in (defaults to the last-used one)").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))),

  new SlashCommandBuilder()
    .setName("applications").setDescription("Configure the application system")
    .addSubcommand(s => s.setName("list").setDescription("List configured applications and their channels/roles"))
    .addSubcommand(s => s.setName("panel").setDescription("Post or refresh an application's panel (Apply button)")
      .addStringOption(o => o.setName("key").setDescription("The application's key, e.g. gambino").setRequired(true))
      .addChannelOption(o => o.setName("channel").setDescription("Channel to post in (defaults to its configured one)").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)))
    .addSubcommand(s => s.setName("setreview").setDescription("Set where submitted applications go for staff review")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addChannelOption(o => o.setName("channel").setDescription("Review channel").setRequired(true).addChannelTypes(ChannelType.GuildText)))
    .addSubcommand(s => s.setName("setpanelchannel").setDescription("Set which channel an application's Apply panel posts to")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addChannelOption(o => o.setName("channel").setDescription("Panel channel").setRequired(true).addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)))
    .addSubcommand(s => s.setName("addrole").setDescription("Add a role granted when an application is accepted")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addRoleOption(o => o.setName("role").setDescription("Role to grant on accept").setRequired(true)))
    .addSubcommand(s => s.setName("removerole").setDescription("Remove an accepted-role from an application")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addRoleOption(o => o.setName("role").setDescription("Role to remove").setRequired(true)))
    .addSubcommand(s => s.setName("setquestions").setDescription("Replace an application's questions")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addStringOption(o => o.setName("questions").setDescription("Questions separated by | (pipe), in order").setRequired(true).setMaxLength(4000)))
    .addSubcommand(s => s.setName("open").setDescription("Open an application so users can apply (or 'all')")
      .addStringOption(o => o.setName("key").setDescription("The application's key, or 'all' for every application").setRequired(true)))
    .addSubcommand(s => s.setName("close").setDescription("Close an application so users can't apply (or 'all')")
      .addStringOption(o => o.setName("key").setDescription("The application's key, or 'all' for every application").setRequired(true))),

  new SlashCommandBuilder()
    .setName("police").setDescription("Police department resources")
    .addSubcommandGroup(g => g.setName("manual").setDescription("Officer guide & procedures manual")
      .addSubcommand(s => s.setName("setup").setDescription("Post the officer guide & procedures manual in a channel")
        .addChannelOption(o => o.setName("channel").setDescription("Channel to post in (defaults to this channel)").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)))),

  new SlashCommandBuilder().setName("help").setDescription("Show all Guardian Bot commands"),
];

// ── Register Commands (GLOBAL - one registration serves every server, present
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

// Guild-scoped commands (e.g. left over from earlier testing/iteration, or a
// stray script) sit ALONGSIDE identically-named global ones and show up as
// duplicates in Discord's command picker for that server. We only ever
// register globally, so wipe any leftover per-guild commands on every guild
// we're currently in.
async function clearStaleGuildCommands() {
  const rest = new REST({ version: "10" }).setToken(TOKEN);
  for (const guild of client.guilds.cache.values()) {
    try {
      const existing = await rest.get(Routes.applicationGuildCommands(CLIENT_ID, guild.id));
      if (!existing.length) continue;
      await rest.put(Routes.applicationGuildCommands(CLIENT_ID, guild.id), { body: [] });
      console.log(`🧹 Cleared ${existing.length} stale guild-scoped command(s) in ${guild.name} (${guild.id}) - was causing duplicates.`);
    } catch (e) { console.error(`⚠️ Failed to clear guild commands for ${guild.name}:`, e.message); }
  }
}

// ── Embed Helpers ─────────────────────────────────────────────
const COLORS = {
  success: 0x00e5a0, warn: 0xf5a623, danger: 0xff3b5c, info: 0x5865f2,
  muted: 0xff7518, nuke: 0xff0033, neutral: 0x2f3136,
};
// Appy-style accent colours for the application DM flow and review embed.
const APPY_GREEN   = 0x57f287; // intro / submitted / accepted (green left bar)
const APPY_BLURPLE = 0x5865f2; // per-question prompts (blurple left bar)
const APPY_RED     = 0xed4245; // denied (red left bar)
const APP_PENDING  = 0xf59e0b; // review pending (orange left bar)

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
  const ownerIds = [...BOT_OWNER_IDS];
  if (ch) ch.send({
    content: ownerIds.map(id => `<@${id}>`).join(" "),
    embeds: [embed(color, desc, title)],
    allowedMentions: { users: ownerIds },
  }).catch(() => {});
  if (config.ownerDM)
    for (const id of ownerIds)
      client.users.fetch(id)
        .then(u => u.send({ embeds: [embed(color, `**[${guild.name}]** ${desc}`, title)] }))
        .catch(() => {});
}

function isOwner(idOrMember) {
  const id = typeof idOrMember === "string" ? idOrMember : idOrMember?.id;
  return BOT_OWNER_IDS.has(id);
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
  if (!target) return { ok: false, why: "I can't find that user in this server." };
  if (isOwner(target)) return { ok: false, why: "That's the bot owner, so they're off-limits." };
  if (target.id === target.guild.ownerId) return { ok: false, why: "That's the server owner - can't touch them." };
  if (isWhitelisted(target)) return { ok: false, why: "That user's whitelisted, so they're protected." };
  if (target.id === actor.id) return { ok: false, why: "You can't do that to yourself." };
  const me = target.guild.members.me;
  if (me && target.roles.highest.position >= me.roles.highest.position)
    return { ok: false, why: "Their top role sits above mine, so I can't. Bump my role higher and try again." };
  const actorPrivileged = isOwner(actor) || actor.id === actor.guild.ownerId;
  if (!actorPrivileged && target.roles.highest.position >= actor.roles.highest.position)
    return { ok: false, why: "Their role is the same as or higher than yours, so this one's out of your reach." };
  return { ok: true };
}

// ── Mod Rate Limit Helpers (scoped + persisted per guild - a mod's limits in
//    one server are independent of, and survive restarts independently of,
//    their activity in any other) ───────────────────────────────────────────
function getModEntry(guildId, userId) {
  if (!modRates[guildId]) modRates[guildId] = {};
  if (!modRates[guildId][userId]) {
    modRates[guildId][userId] = { bans: [], kicks: [], mutes: [], purges: [], lockdowns: [], warns: [] };
  }
  return modRates[guildId][userId];
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
  saveModRates(guildId);
}
function limitDeniedEmbed(action, used, limit, resetsInMin) {
  return embed(COLORS.danger,
    `You've hit your \`/${action}\` limit for now.\n\n` +
    `That's **${used}/${limit}** ${action}s in the last ${config.modWindowMs / 3600000}h. ` +
    `You'll be able to use it again in about **${resetsInMin} minute${resetsInMin === 1 ? "" : "s"}**.`);
}
function usageFooter(action, used, limit) {
  const remaining = limit - used;
  const bar = buildBar(used, limit, 10);
  const warning = remaining <= Math.ceil(limit * 0.2) && remaining > 0
    ? `\nJust **${remaining}** ${action}${remaining === 1 ? "" : "s"} remaining today.` : "";
  return `\`${bar}\` **${used}/${limit}** ${action}s used today${warning}`;
}
function buildBar(used, limit, width = 10) {
  const filled = Math.min(width, Math.round((used / Math.max(limit, 1)) * width));
  return "█".repeat(filled) + "░".repeat(width - filled);
}
function formatUptime(ms) {
  const s = Math.floor(ms / 1000);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}d ${h}h ${m}m`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m ${s % 60}s`;
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
    if (strippedIds.length) await member.roles.remove(strippedIds, `Mute: stash roles - ${reason}`);
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
    `<@${member.id}> was muted for **${durationMin > 0 ? durationMin + " min" : "as long as it takes"}** - ${reason}\n` +
    `I set aside **${stash.length}** role${stash.length === 1 ? "" : "s"} to give back on unmute: ${stash.length ? stash.map(id => `<@&${id}>`).join(", ") : "none"}` +
    (unstrippable.size ? `\nCouldn't take these (managed or above me): ${unstrippable.map(r => `<@&${r.id}>`).join(", ")}` : ""),
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
      if (restorable.length) await member.roles.add(restorable, `Restore stashed roles - ${reason}`).catch(() => {});
      secLog(guild, "Roles Restored",
        `<@${userId}> is unmuted, and I gave back **${restorable.length}** role${restorable.length === 1 ? "" : "s"}: ${restorable.length ? restorable.map(id => `<@&${id}>`).join(", ") : "none"}` +
        (lost.length ? `\nCouldn't restore these (deleted or above me): ${lost.map(id => `<@&${id}>`).join(", ")}` : "") +
        `\n_(${reason})_`, COLORS.success);
    } else {
      secLog(guild, "Member Unmuted", `<@${userId}> is unmuted. There were no stashed roles to give back. _(${reason})_`);
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
      if (data.expiresAt == null) continue; // permanent - leave for manual /unmute
      const remaining = data.expiresAt - Date.now();
      if (remaining <= 0) unmuteUser(guild, userId, "Auto-unmute (expired during downtime)");
      else scheduleTask(() => unmuteUser(guild, userId, "Auto-unmute (timer, resumed post-restart)"), remaining);
    }
  }
}

// ── Boot recovery: reschedule / expire raid lockdowns; leave panic/nukestorm
//    lockdowns active (they have no auto-expiry - same as before a restart) ──
async function recoverLockdowns() {
  for (const [guildId, state] of Object.entries(lockdownState)) {
    const guild = client.guilds.cache.get(guildId);
    if (!guild) continue;
    if (state.expiresAt == null) continue; // manual (panic/nukestorm) - stays locked until /panic or manual unlock
    const remaining = state.expiresAt - Date.now();
    if (remaining <= 0) await liftLockdownChannels(guild, "Auto-lifted (timer expired during downtime).");
    else scheduleTask(() => liftLockdownChannels(guild, "Auto-lifted (timer, resumed post-restart)."), remaining);
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
    secLog(message.guild, "Anti-Spam", `Muted <@${uid}> for mass-mentioning (${mentionCount}) in <#${message.channel.id}>.`, COLORS.warn);
    return true;
  }

  // Scam / phishing / IP-grabber links
  if (config.scamBlock && SCAM_RE.test(message.content)) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, "Anti-spam: scam/grabber link");
    alertOwner(message.guild, `Heads up - <@${uid}> dropped what looks like a **scam or grabber link** in <#${message.channel.id}>. I've deleted it and muted them.`, COLORS.danger, "Scam Link Blocked");
    return true;
  }

  // Invite-link spam
  if (config.spamBlockInvites && INVITE_RE.test(message.content) && !isMod(message.member)) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, "Anti-spam: posted invite link");
    secLog(message.guild, "Anti-Spam", `Muted <@${uid}> for posting an invite link in <#${message.channel.id}>.`, COLORS.warn);
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
      secLog(message.guild, "Anti-Spam", `Muted <@${uid}> for flooding the same message over and over in <#${message.channel.id}>.`, COLORS.warn);
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
    secLog(message.guild, "Anti-Spam", `Muted <@${uid}> for flooding <#${message.channel.id}> with messages.`, COLORS.warn);
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
    `<@${member.id}> pinged ${targets} in <#${message.channel.id}>, so they were **${actionText}**.`, COLORS.warn);
}

// ── Anti-Raid (join velocity + new-account quarantine) ────────
client.on(Events.GuildMemberAdd, async (member) => {
  const now = Date.now();
  const gid = member.guild.id;

  // Quarantine brand-new accounts that join while THIS guild's raid lockdown is active.
  if (isLockdown(gid) && config.raidKickNewOnLock && !member.user.bot) {
    const ageMin = (now - member.user.createdTimestamp) / 60000;
    if (ageMin < config.raidMinAccountAgeMin) {
      await tryDM(member.user, "The server's in a temporary raid lockdown right now, so I couldn't let you in. Please try joining again a little later.");
      await member.kick(`Raid lockdown: new account (${Math.round(ageMin)}m old)`).catch(() => {});
      secLog(member.guild, "Raid Quarantine", `Turned away <@${member.id}> during the lockdown - it's a brand-new account (${Math.round(ageMin)}m old).`, COLORS.danger);
      return;
    }
  }

  const joins = (joinTracker.get(gid) || []).filter(t => now - t < config.raidWindowMs);
  joins.push(now);
  joinTracker.set(gid, joins);
  const recent = joins.length;
  if (recent >= config.raidJoinThreshold && !isLockdown(gid)) {
    const expiresAt = Date.now() + config.raidLockdownMin * 60000;
    setLockdown(gid, "raid", expiresAt);
    alertOwner(member.guild, `Looks like a raid - **${recent}** people joined in just ${config.raidWindowMs / 1000}s. I've locked the server down for **${config.raidLockdownMin} min** to be safe.`, COLORS.nuke, "Raid Detected");
    member.guild.channels.cache.forEach(ch => {
      if (ch.isTextBased() && !ch.isThread()) ch.permissionOverwrites.edit(member.guild.roles.everyone, { SendMessages: false }).catch(() => {});
    });
    scheduleTask(() => liftLockdownChannels(member.guild, `Lifted the raid lockdown automatically after **${config.raidLockdownMin} minutes**. Things should be back to normal.`), expiresAt - Date.now());
  }
});

// ── Anti-Nuke engine (scoped per guild - a user's actions in one server never
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
    `Anti-nuke just kicked in on <@${member.id}> (\`${member.id}\`).\n**What set it off:** ${reason}\n**What I did:** pulled their dangerous roles and moved to ban them.`,
    COLORS.nuke, "Anti-Nuke Triggered");

  try {
    const toRemove = member.roles.cache.filter(r => r.permissions.any(DANGER_PERMS) && r.editable);
    if (toRemove.size > 0) await member.roles.remove([...toRemove.keys()], "Anti-nuke: role strip");
  } catch (e) {
    secLog(guild, "Anti-Nuke", `I couldn't pull the roles off <@${member.id}>: ${e.message}`, COLORS.warn);
  }

  try {
    await member.ban({ reason: `Anti-Nuke: ${reason}` });
    secLog(guild, "Anti-Nuke", `Banned <@${member.id}> - ${reason}`, COLORS.nuke);
  } catch (e) {
    // Ban failed (likely above the bot). Try kick; otherwise leave de-permed + escalate.
    const kicked = await member.kick(`Anti-Nuke: ${reason}`).catch(() => null);
    alertOwner(guild,
      `I couldn't ban <@${member.id}> (${e.message}). ` +
      (kicked === null ? `The kick didn't go through either, so I've only managed to strip their roles. **Please check my role position right away.**` : `I kicked them instead.`),
      COLORS.danger, "Anti-Nuke Needs a Look");
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
        alertOwner(guild, `<@${executorId}> just handed <@&${targetId}> some dangerous permissions. I've rolled that back.`, COLORS.warn, "Permission Change Reverted");
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
        alertOwner(guild, `<@${executorId}> just gave <@${targetId}> some dangerous role(s): ${dangerous.map(r => `<@&${r.id}>`).join(", ")}. I've taken them back off.`, COLORS.warn, "Role Grant Reverted");
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
          await executor.roles.remove(strippedIds, "Anti-nuke: added a bot - roles stripped").catch(() => {});

        const unstrippable = executor.roles.cache.filter(r =>
          r.id !== guild.id && (r.managed || !r.editable));

        alertOwner(guild,
          `<@${executorId}> added the bot <@${targetId}> - ${config.nukeBotAddAction === "kick" ? "I've kicked it back out." : "you'll want to review this."}\n` +
          `I also pulled **${strippedIds.length}** role${strippedIds.length === 1 ? "" : "s"} off <@${executorId}>: ${strippedIds.length ? strippedIds.map(id => `<@&${id}>`).join(", ") : "none"}` +
          (unstrippable.size ? `\nCouldn't take these (managed or above me): ${unstrippable.map(r => `<@&${r.id}>`).join(", ")}` : ""),
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
        alertOwner(guild, `<@${executorId}> changed the server settings. Might be worth a glance at the audit log.`, COLORS.warn, "Server Settings Changed");
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
const HIDDEN_OWNER_COMMANDS = new Set(["!failsafe", "!restore", "!snapshot", "!snapshots", "!rollback", "!ownerhelp"]);
client.on(Events.MessageCreate, async (message) => {
  if (!message.guild || !isOwner(message.author.id)) return;
  const cmd = message.content.trim().toLowerCase();
  if (!HIDDEN_OWNER_COMMANDS.has(cmd)) return;
  // Full audit trail: every invocation of a hidden owner command, regardless of outcome.
  appendForensic(message.guild.id, "owner_command", { cmd, by: message.author.id });
  try {
    if (cmd === "!failsafe") return await runFailsafe(message);
    if (cmd === "!restore")  return await runRestore(message);
    if (cmd === "!snapshot") {
      const r = await snapshotGuild(message.guild);
      const kept = (snapshots[message.guild.id] || []).length;
      secLog(message.guild, "Snapshot Taken", `<@${message.author.id}> took a manual snapshot - **${r.roles}** roles, **${r.channels}** channels.`, COLORS.success);
      return message.reply(`📸 Snapshot saved - **${r.roles}** roles, **${r.channels}** channels. (${kept}/${config.snapshotMax} kept)`);
    }
    if (cmd === "!snapshots") {
      const arr = snapshots[message.guild.id] || [];
      if (!arr.length) return message.reply("No snapshots yet. Run `!snapshot`.");
      const lines = arr.map((s, i) => `**${i + 1}.** <t:${Math.floor(s.takenAt / 1000)}:R> - ${s.roles.length} roles, ${s.channels.length} channels`).join("\n");
      return message.reply(`📸 **Snapshots (newest last):**\n${lines}`);
    }
    if (cmd === "!rollback") return await rollbackGuild(message.guild, message);
    if (cmd === "!ownerhelp") {
      return message.reply(
        "🛡️ **Hidden owner commands** (only you can run these):\n" +
        "`!failsafe` - back up + delete the target roles and kick all bots\n" +
        "`!restore` - rebuild those roles (perms, position, channel access, members)\n" +
        "`!snapshot` - take a full-guild snapshot now\n" +
        "`!snapshots` - list stored snapshots\n" +
        "`!rollback` - **destructive**: restore the server to exactly match the latest snapshot - deletes roles/channels not in it, corrects drifted permissions, re-syncs role membership. Asks for ✅ confirmation first.");
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
    .setTitle(`🛡️ Guardian setup - ${guild.name}`)
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
      { name: "Failsafe Roles", value: g.failsafeRoleIds.length ? g.failsafeRoleIds.map(id => `<@&${id}>`).join(", ") : "None - configure with `/setup failsafe`", inline: false },
    )
    .setFooter({ text: "Behavioral thresholds are global (.env); these identity settings are per-server." })
    .setTimestamp();
}

// /setup quick - auto-provision a working Muted role + Guardian log category/channels
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

// ── Ticket System ────────────────────────────────────────────
function buildTicketPanelEmbed(guild, cfg) {
  return new EmbedBuilder()
    .setColor(COLORS.info)
    .setTitle("🎫 Support Tickets")
    .setDescription(
      "Need a hand? Pick the option below that fits what you need, and I'll open a private " +
      "ticket just for you and the team.\n\n" +
      cfg.types.map(t => `${t.emoji || "🎫"}  **${t.label}**`).join("\n") +
      "\n\nSomeone will be with you as soon as they can. Please stick to one ticket at a time."
    )
    .setThumbnail(guild.iconURL?.() || null)
    .setFooter({ text: guild.name })
    .setTimestamp();
}
function buildTicketPanelRows(cfg) {
  const buttons = cfg.types.slice(0, 25).map(t =>
    new ButtonBuilder().setCustomId(`ticket_open_${t.key}`).setLabel(t.label).setEmoji(t.emoji || "🎫").setStyle(ButtonStyle.Secondary));
  const rows = [];
  for (let i = 0; i < buttons.length; i += 5) rows.push(new ActionRowBuilder().addComponents(buttons.slice(i, i + 5)));
  return rows;
}

// Post the panel (or leave it alone if it's already posted and the message
// still exists) - called on boot for every guild with ticket types configured.
async function ensureTicketPanel(guild) {
  const cfg = getTicketConfig(guild.id);
  if (!cfg.types.length || !cfg.panelChannelId) return;
  const channel = guild.channels.cache.get(cfg.panelChannelId);
  if (!channel) return;
  if (cfg.panelMessageId) {
    const existing = await channel.messages.fetch(cfg.panelMessageId).catch(() => null);
    if (existing) return;
  }
  const posted = await channel.send({ embeds: [buildTicketPanelEmbed(guild, cfg)], components: buildTicketPanelRows(cfg) }).catch(() => null);
  if (posted) {
    setTicketConfig(guild.id, { panelMessageId: posted.id });
    console.log(`🎫 Posted ticket panel in #${channel.name} (${guild.name})`);
  }
}

function escapeHtml(str) {
  return String(str ?? "").replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

// Paginate through the whole channel history, oldest first.
async function fetchAllMessages(channel) {
  let all = []; let lastId;
  for (;;) {
    const batch = await channel.messages.fetch({ limit: 100, ...(lastId ? { before: lastId } : {}) }).catch(() => null);
    if (!batch || !batch.size) break;
    all = all.concat([...batch.values()]);
    lastId = batch.last().id;
    if (batch.size < 100) break;
  }
  return all.reverse();
}

// Self-contained, dependency-free HTML transcript (dark-themed to resemble Discord).
async function buildTranscript(channel, ticket, type, closerTag) {
  const messages = await fetchAllMessages(channel);
  const rows = messages.map(m => {
    const time = new Date(m.createdTimestamp).toISOString().replace("T", " ").slice(0, 19) + " UTC";
    const author = escapeHtml(m.author?.tag || "Unknown");
    const avatar = escapeHtml(m.author?.displayAvatarURL?.({ size: 64 }) || "");
    const content = escapeHtml(m.content || "").replace(/\n/g, "<br>");
    const atts = [...m.attachments.values()]
      .map(a => `<div class="att"><a href="${escapeHtml(a.url)}" target="_blank" rel="noopener">📎 ${escapeHtml(a.name || "attachment")}</a></div>`)
      .join("");
    return `<div class="msg">${avatar ? `<img class="avatar" src="${avatar}">` : `<div class="avatar"></div>`}<div class="body"><div class="meta"><span class="author">${author}</span><span class="time">${time}</span></div><div class="content">${content || "<i>(no text content)</i>"}</div>${atts}</div></div>`;
  }).join("\n");

  return `<!doctype html>
<html><head><meta charset="utf-8"><title>Transcript - #${escapeHtml(channel.name)}</title>
<style>
  body { background:#313338; color:#dbdee1; font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif; margin:0; padding:24px; }
  .header { border-bottom:1px solid #3f4147; padding-bottom:16px; margin-bottom:16px; }
  .header h1 { margin:0 0 4px; font-size:20px; color:#f2f3f5; }
  .header .sub { color:#949ba4; font-size:13px; }
  .msg { display:flex; gap:12px; padding:8px 0; }
  .avatar { width:40px; height:40px; border-radius:50%; flex-shrink:0; background:#5865f2; }
  .meta { font-size:13px; margin-bottom:2px; }
  .author { font-weight:600; color:#f2f3f5; }
  .time { color:#949ba4; margin-left:8px; }
  .content { font-size:15px; line-height:1.4; white-space:pre-wrap; word-wrap:break-word; }
  .att { margin-top:4px; }
  .att a { color:#00a8fc; text-decoration:none; }
</style></head>
<body>
  <div class="header">
    <h1>🎫 ${escapeHtml(type?.label || ticket.typeKey)} - #${escapeHtml(channel.name)}</h1>
    <div class="sub">Opened by &lt;${escapeHtml(ticket.openerId)}&gt; · Closed by ${escapeHtml(closerTag || "unknown")} · ${messages.length} message(s)</div>
  </div>
  ${rows || "<p><i>No messages were sent in this ticket.</i></p>"}
</body></html>`;
}

async function createTicketChannel(interaction, key, reason) {
  const { guild, member } = interaction;
  const cfg = getTicketConfig(guild.id);
  const type = cfg.types.find(t => t.key === key);
  if (!type) return interaction.reply({ content: "Sorry, that ticket option isn't available anymore.", ephemeral: true });

  const existing = findOpenTicketByUser(guild.id, member.id, key);
  if (existing && guild.channels.cache.has(existing))
    return interaction.reply({ content: `You've already got one open over here: <#${existing}>`, ephemeral: true });

  await interaction.deferReply({ ephemeral: true });

  let category = cfg.categoryId ? guild.channels.cache.get(cfg.categoryId) : null;
  if (!category) {
    category = guild.channels.cache.find(c => c.type === ChannelType.GuildCategory && c.name === "Tickets");
    if (!category) category = await guild.channels.create({ name: "Tickets", type: ChannelType.GuildCategory, reason: "Ticket system: auto-created category" }).catch(() => null);
    if (category) setTicketConfig(guild.id, { categoryId: category.id });
  }

  // Explicit `type` (0 = role, 1 = member) on every overwrite - without it,
  // discord.js tries to guess by checking caches and throws "Supplied
  // parameter is not a cached User or Role" whenever it can't resolve one
  // (e.g. a modRoleId that isn't cached at that instant).
  const g = gc(guild);
  const overwrites = [
    { id: guild.id, type: 0, deny: [PermissionsBitField.Flags.ViewChannel] },
    { id: member.id, type: 1, allow: [PermissionsBitField.Flags.ViewChannel, PermissionsBitField.Flags.SendMessages, PermissionsBitField.Flags.ReadMessageHistory, PermissionsBitField.Flags.AttachFiles] },
  ];
  if (g.modRoleId) overwrites.push({ id: g.modRoleId, type: 0, allow: [PermissionsBitField.Flags.ViewChannel, PermissionsBitField.Flags.SendMessages, PermissionsBitField.Flags.ReadMessageHistory, PermissionsBitField.Flags.ManageMessages] });

  const safeName = (member.user.username || "user").toLowerCase().replace(/[^a-z0-9-]/g, "-").slice(0, 20) || "user";
  const channelName = `${type.key.replace(/_/g, "-")}-${safeName}`.slice(0, 90);

  let createErr = null;
  let ticketChannel = await guild.channels.create({
    name: channelName, type: ChannelType.GuildText, parent: category?.id,
    permissionOverwrites: overwrites, reason: `Ticket opened by ${member.user.tag}`,
    topic: `${type.label} ticket for ${member.user.tag} (${member.id})`,
  }).catch(e => { createErr = e; return null; });

  // If it failed while assigned to a category, retry once without a parent -
  // covers a full/invalid/stale category without fully blocking ticket creation.
  if (!ticketChannel && category) {
    ticketChannel = await guild.channels.create({
      name: channelName, type: ChannelType.GuildText,
      permissionOverwrites: overwrites, reason: `Ticket opened by ${member.user.tag}`,
      topic: `${type.label} ticket for ${member.user.tag} (${member.id})`,
    }).catch(e => { createErr = e; return null; });
  }

  if (!ticketChannel)
    return interaction.editReply(`Hmm, I couldn't open a ticket channel: \`${createErr?.message || "unknown error"}\`. Please double-check I have the Manage Channels permission.`);

  setOpenTicket(guild.id, ticketChannel.id, { typeKey: key, openerId: member.id, openedAt: Date.now(), claimedBy: null, reason });

  const welcomeEmbed = new EmbedBuilder()
    .setColor(COLORS.info)
    .setTitle(`${type.emoji || "🎫"} ${type.label}`)
    .setDescription(`Thanks for reaching out, <@${member.id}> - someone from the team will be with you shortly. Here's what you told us:\n\n${reason}`)
    .addFields(
      { name: "Opened by", value: `<@${member.id}>`, inline: true },
      { name: "Category", value: type.label, inline: true },
      { name: "Status", value: "🟢 Open, waiting for staff", inline: true },
    )
    .setFooter({ text: `Ticket ID: ${ticketChannel.id}` })
    .setTimestamp();
  const controlRow = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId("ticket_claim").setLabel("Claim").setEmoji("🙋").setStyle(ButtonStyle.Primary),
    new ButtonBuilder().setCustomId("ticket_close").setLabel("Close Ticket").setEmoji("🔒").setStyle(ButtonStyle.Danger),
  );
  const pingRole = g.modRoleId ? `<@&${g.modRoleId}> ` : "";
  await ticketChannel.send({ content: `${pingRole}<@${member.id}>`, embeds: [welcomeEmbed], components: [controlRow] }).catch(() => {});

  secLog(guild, "Ticket Opened", `<@${member.id}> opened a **${type.label}** ticket over in <#${ticketChannel.id}>.`, COLORS.info);
  return interaction.editReply(`You're all set - your ticket's open here: <#${ticketChannel.id}>`);
}

async function handleTicketOpen(interaction) {
  const { guild, customId } = interaction;
  const key = customId.replace("ticket_open_", "");
  const cfg = getTicketConfig(guild.id);
  const type = cfg.types.find(t => t.key === key);
  if (!type) return interaction.reply({ content: "Sorry, that ticket option isn't available anymore.", ephemeral: true });

  const existing = findOpenTicketByUser(guild.id, interaction.member.id, key);
  if (existing && guild.channels.cache.has(existing))
    return interaction.reply({ content: `You've already got one open over here: <#${existing}>`, ephemeral: true });

  const modal = new ModalBuilder().setCustomId(`ticket_reason_${key}`).setTitle(`${type.label} - Ticket`.slice(0, 45));
  const reasonInput = new TextInputBuilder()
    .setCustomId("reason").setLabel("What can we help you with?").setStyle(TextInputStyle.Paragraph)
    .setRequired(true).setMaxLength(1000).setPlaceholder("A few details go a long way (who, what, when)...");
  modal.addComponents(new ActionRowBuilder().addComponents(reasonInput));
  return interaction.showModal(modal);
}

async function handleTicketClaim(interaction) {
  const { guild, member, channel } = interaction;
  const ticket = getOpenTicket(guild.id, channel.id);
  if (!ticket) return interaction.reply({ content: "This isn't an active ticket channel.", ephemeral: true });
  if (!isMod(member)) return interaction.reply({ content: "Only staff can claim tickets.", ephemeral: true });
  if (ticket.claimedBy) return interaction.reply({ content: `This one's already claimed by <@${ticket.claimedBy}>.`, ephemeral: true });

  ticket.claimedBy = member.id;
  setOpenTicket(guild.id, channel.id, ticket);

  const oldEmbed = interaction.message.embeds[0];
  if (oldEmbed) {
    const newEmbed = EmbedBuilder.from(oldEmbed).spliceFields(2, 1, { name: "Status", value: `🟡 Claimed by <@${member.id}>`, inline: true });
    await interaction.update({ embeds: [newEmbed] }).catch(() => interaction.deferUpdate().catch(() => {}));
  } else {
    await interaction.deferUpdate().catch(() => {});
  }
  await channel.send({ embeds: [embed(COLORS.warn, `<@${member.id}> has got this one and will help you out from here.`)] }).catch(() => {});
}

async function handleTicketClose(interaction) {
  const { guild, member, channel } = interaction;
  const ticket = getOpenTicket(guild.id, channel.id);
  if (!ticket) return interaction.reply({ content: "This isn't an active ticket channel.", ephemeral: true });
  if (!isMod(member) && member.id !== ticket.openerId)
    return interaction.reply({ content: "Only staff or the person who opened this can close it.", ephemeral: true });

  await interaction.reply({ embeds: [embed(COLORS.warn, "Closing this ticket and saving a transcript, one sec...")] }).catch(() => {});

  const cfg = getTicketConfig(guild.id);
  const type = cfg.types.find(t => t.key === ticket.typeKey);
  const transcript = await buildTranscript(channel, ticket, type, member.user.tag);

  const logChannel = type?.logChannelId ? guild.channels.cache.get(type.logChannelId) : null;
  const openerUser = await client.users.fetch(ticket.openerId).catch(() => null);
  const summaryEmbed = new EmbedBuilder()
    .setColor(COLORS.neutral)
    .setTitle(`🔒 Ticket Closed - ${type?.label || ticket.typeKey}`)
    .addFields(
      { name: "Opened by", value: openerUser ? `${openerUser.tag} (\`${ticket.openerId}\`)` : `\`${ticket.openerId}\``, inline: true },
      { name: "Closed by", value: `<@${member.id}>`, inline: true },
      { name: "Claimed by", value: ticket.claimedBy ? `<@${ticket.claimedBy}>` : "Unclaimed", inline: true },
      { name: "Opened", value: `<t:${Math.floor(ticket.openedAt / 1000)}:F>`, inline: true },
      { name: "Duration", value: formatUptime(Date.now() - ticket.openedAt), inline: true },
      { name: "Reason", value: (ticket.reason || "N/A").slice(0, 1024), inline: false },
    )
    .setTimestamp();

  if (logChannel) {
    await logChannel.send({
      embeds: [summaryEmbed],
      files: [{ attachment: Buffer.from(transcript, "utf8"), name: `transcript-${channel.name}.html` }],
    }).catch(() => {});
  }

  secLog(guild, "Ticket Closed", `<@${member.id}> closed the **${type?.label || ticket.typeKey}** ticket that <@${ticket.openerId}> opened (<#${channel.id}>).`, COLORS.neutral);
  deleteOpenTicket(guild.id, channel.id);

  await channel.send("All done here - this channel will disappear in a few seconds.").catch(() => {});
  setTimeout(() => channel.delete("Ticket closed").catch(() => {}), 5000);
}

// ── Application System (Appy-style DM interview → staff review → role grant) ──
const APP_QUESTION_TIMEOUT_MS = 10 * 60 * 1000; // per-question DM reply window
// Users with an in-progress DM interview, so we never start two at once.
const activeDmApps = new Set(); // userId
// Per-answer character cap: spread a safe budget across the questions so the
// finished review embed stays under Discord's 6000-char total, capped at the
// 1024 per-field limit.
function appAnswerCap(questionCount) {
  return Math.max(200, Math.min(1024, Math.floor(5200 / Math.max(questionCount, 1))));
}

// Requirements block shown as an application panel's description. Age and
// member-time minimums are per-app (app.minAge / app.minMemberTime), so each
// application can state its own; both fall back to sensible defaults.
function buildRequirements(app) {
  const age = app?.minAge ?? 14;
  const memberTime = app?.minMemberTime || "1 week";
  return "**REQUIREMENTS**\n" +
    `Age: ${age}\n` +
    "No Joke Applications (May result in blacklist)\n" +
    "Use of AI is not tolerated\n" +
    `Must be a member longer than ${memberTime}`;
}

function buildAppPanelEmbed(guild, app) {
  const closed = !!app.closed;
  const e = new EmbedBuilder()
    .setColor(closed ? COLORS.neutral : COLORS.info)
    .setTitle(`${app.emoji || "📝"} ${app.label} Application${closed ? " (Closed)" : ""}`)
    .setThumbnail(guild.iconURL?.() || null)
    .setFooter({ text: guild.name })
    .setTimestamp();
  if (closed) {
    e.setDescription(`**${app.label} applications are closed right now.** Check back soon.\n\n${buildRequirements(app)}`);
  } else {
    e.setDescription(buildRequirements(app));
  }
  return e;
}
// A single Apply button reflecting one app's open/closed state.
function buildApplyButton(app) {
  const closed = !!app.closed;
  return new ButtonBuilder()
    .setCustomId(`app_apply_${app.key}`)
    .setLabel(closed ? `${app.label} closed`.slice(0, 80) : `Apply for ${app.label}`.slice(0, 80))
    .setEmoji(closed ? "🔒" : (app.emoji || "📝"))
    .setStyle(closed ? ButtonStyle.Secondary : ButtonStyle.Primary)
    .setDisabled(closed);
}

// Combined panel embed for a channel that hosts 2+ applications (e.g. the
// family channel with Gambino + Colombo) - one embed, a button per app.
// If every app shares the same requirements, show one block; otherwise show
// each app's requirements under its own heading.
function buildCombinedPanelEmbed(guild, apps) {
  const blocks = apps.map(a => [a, buildRequirements(a)]);
  const unique = [...new Set(blocks.map(([, r]) => r))];
  const description = unique.length === 1
    ? unique[0]
    : blocks.map(([a, r]) => `${a.emoji || "📝"} __${a.label}__\n${r}`).join("\n\n");
  return new EmbedBuilder()
    .setColor(COLORS.info)
    .setTitle("📋 Applications")
    .setDescription(description)
    .setThumbnail(guild.iconURL?.() || null)
    .setFooter({ text: guild.name })
    .setTimestamp();
}

// Group a guild's panel-eligible apps by their panel channel.
function appsByPanelChannel(guildId) {
  const groups = new Map(); // channelId -> [apps]
  for (const app of Object.values(getApplications(guildId))) {
    if (!app.panelChannelId || !app.questions?.length) continue;
    if (!groups.has(app.panelChannelId)) groups.set(app.panelChannelId, []);
    groups.get(app.panelChannelId).push(app);
  }
  return groups;
}

// Message payload for a channel's panel: single-app style for one app, a
// combined embed with one button per app for a shared channel.
function panelPayloadForGroup(guild, apps) {
  if (apps.length === 1) return { embeds: [buildAppPanelEmbed(guild, apps[0])], components: [new ActionRowBuilder().addComponents(buildApplyButton(apps[0]))] };
  const buttons = apps.slice(0, 25).map(buildApplyButton);
  const rows = [];
  for (let i = 0; i < buttons.length; i += 5) rows.push(new ActionRowBuilder().addComponents(buttons.slice(i, i + 5)));
  return { embeds: [buildCombinedPanelEmbed(guild, apps)], components: rows };
}

// Point every app in a channel group at the same panel message id.
function setGroupPanelMessage(guildId, apps, messageId) {
  for (const a of apps) if (a.panelMessageId !== messageId) setApplication(guildId, a.key, { panelMessageId: messageId });
}

// Render (edit-in-place or post) the one panel message for a channel group, so
// open/close changes on any member app update the shared panel live.
async function renderChannelPanel(guild, channelId, apps) {
  const channel = guild.channels.cache.get(channelId);
  if (!channel) return;
  const payload = panelPayloadForGroup(guild, apps);
  const existingId = apps.map(a => a.panelMessageId).find(Boolean);
  if (existingId) {
    const existing = await channel.messages.fetch(existingId).catch(() => null);
    if (existing) { await existing.edit(payload).catch(() => {}); setGroupPanelMessage(guild.id, apps, existing.id); return; }
  }
  const posted = await channel.send(payload).catch(() => null);
  if (posted) setGroupPanelMessage(guild.id, apps, posted.id);
}

// Refresh the whole panel of the channel `app` lives in (so a combined panel's
// other buttons are rebuilt too when this one's open/closed state changes).
async function refreshAppPanel(guild, app) {
  if (!app.panelChannelId) return;
  const apps = appsByPanelChannel(guild.id).get(app.panelChannelId) || [app];
  await renderChannelPanel(guild, app.panelChannelId, apps);
}

// Post each channel's panel if it isn't already up. For a shared channel this
// also reconciles any leftover separate/duplicate panels (e.g. from before
// Gambino + Colombo were combined) down to a single combined message.
async function ensureApplicationPanels(guild) {
  for (const [channelId, apps] of appsByPanelChannel(guild.id)) {
    const channel = guild.channels.cache.get(channelId);
    if (!channel) continue;
    const ids = [...new Set(apps.map(a => a.panelMessageId).filter(Boolean))];
    const live = [];
    for (const id of ids) { const m = await channel.messages.fetch(id).catch(() => null); if (m) live.push(m); }

    // Already a single shared panel message - just refresh it to the current state.
    if (live.length === 1 && ids.length === 1) {
      await live[0].edit(panelPayloadForGroup(guild, apps)).catch(() => {});
      setGroupPanelMessage(guild.id, apps, live[0].id);
      continue;
    }
    // Otherwise (nothing up yet, or multiple stale/separate panels): clear any
    // leftovers and post one fresh panel for the channel.
    for (const m of live) await m.delete().catch(() => {});
    const posted = await channel.send(panelPayloadForGroup(guild, apps)).catch(() => null);
    if (posted) {
      setGroupPanelMessage(guild.id, apps, posted.id);
      console.log(`📝 Posted application panel (${apps.map(a => a.label).join(", ")}) in #${channel.name} (${guild.name})`);
    }
  }
}

async function handleAppApply(interaction) {
  const key = interaction.customId.replace("app_apply_", "");
  const app = getApplication(interaction.guild.id, key);
  if (!app) return interaction.reply({ content: "Sorry, that application isn't around anymore.", ephemeral: true });
  // Re-check even though the button is disabled when closed - the panel message
  // could be stale, so never let a closed application start an interview.
  if (app.closed) {
    await refreshAppPanel(interaction.guild, app).catch(() => {}); // resync the stale panel
    return interaction.reply({ content: `**${app.label}** applications are closed right now. Do check back soon!`, ephemeral: true });
  }
  if (!app.reviewChannelId) return interaction.reply({ content: "This application isn't quite ready yet. Please give an admin a heads up.", ephemeral: true });
  if (!app.questions?.length) return interaction.reply({ content: "This application doesn't have any questions set up yet. Please let an admin know.", ephemeral: true });
  if (activeDmApps.has(interaction.user.id))
    return interaction.reply({ content: "You've already got an application open in your DMs. Finish that one first, or hit **Cancel Application** there, then come back.", ephemeral: true });

  // Open a DM and send the intro BEFORE acknowledging, so a closed-DM user gets a
  // clear message instead of silently starting an interview they can't see.
  let dm, introMsg;
  try {
    dm = await interaction.user.createDM();
    introMsg = await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_GREEN)
      .setTitle("Application Started")
      .setDescription("Just answer the questions below by sending a message to the bot. Take your time, and be honest.")] });
  } catch {
    return interaction.reply({ content: "I couldn't slide into your DMs. Turn on direct messages for this server (Privacy Settings → Allow direct messages from server members), then give Apply another tap.", ephemeral: true });
  }

  // Appy-style ephemeral confirmation: a green "Application started" card with a
  // Jump-to-application link button pointing at the DM.
  const jumpRow = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setStyle(ButtonStyle.Link).setLabel("Jump to application")
      .setURL(`https://discord.com/channels/@me/${dm.id}/${introMsg.id}`));
  await interaction.reply({ ephemeral: true, components: [jumpRow], embeds: [new EmbedBuilder().setColor(APPY_GREEN)
    .setTitle("Application started")
    .setDescription("Your application's up and waiting in your DMs. Hit the button below to jump straight to it.")] });
  runDmApplication(interaction.guild, interaction.user, app, dm).catch(err => console.error("⚠️ DM application flow failed:", err));
}

// Walk the applicant through the questions in DMs, one at a time (Appy-style).
async function runDmApplication(guild, user, app, dm) {
  activeDmApps.add(user.id);
  const startedAt = Date.now();
  try {
    const cap = appAnswerCap(app.questions.length);
    const total = app.questions.length;
    const cancelRow = new ActionRowBuilder().addComponents(
      new ButtonBuilder().setCustomId("app_cancel").setLabel("Cancel Application").setStyle(ButtonStyle.Danger));
    const answers = [];

    for (let i = 0; i < total; i++) {
      const qMsg = await dm.send({
        embeds: [new EmbedBuilder().setColor(APPY_BLURPLE)
          .setTitle(`${app.label} Application`)
          .setDescription(`${i + 1}/${total}. ${app.questions[i]}\n\n-# To answer this one, just send your response as a message here.`)],
        components: [cancelRow],
      }).catch(() => null);

      // Whichever comes first: the applicant's reply, or a click on Cancel Application.
      const replyP = dm.awaitMessages({ filter: m => m.author.id === user.id, max: 1, time: APP_QUESTION_TIMEOUT_MS })
        .then(c => (c.size ? c.first() : "TIMEOUT")).catch(() => "TIMEOUT");
      const cancelP = qMsg
        ? qMsg.awaitMessageComponent({ filter: b => b.user.id === user.id && b.customId === "app_cancel", time: APP_QUESTION_TIMEOUT_MS })
            .then(b => { b.deferUpdate().catch(() => {}); return "CANCEL"; }).catch(() => "TIMEOUT")
        : Promise.resolve("TIMEOUT");
      const result = await Promise.race([replyP, cancelP]);
      if (qMsg) await qMsg.edit({ components: [] }).catch(() => {}); // retire the Cancel button for this question

      if (result === "CANCEL") {
        await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Application cancelled")
          .setDescription(`All good, I've scrapped your ${app.label} application. Nothing got sent. Swing by the panel whenever you want to give it another go.`)] }).catch(() => {});
        return;
      }
      if (result === "TIMEOUT") {
        await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Application cancelled")
          .setDescription(`Looks like you wandered off, so I've closed out your ${app.label} application for now. Start fresh from the panel whenever you're ready.`)] }).catch(() => {});
        return;
      }

      const msg = result;
      let content = (msg.content || "").trim();
      if (content.toLowerCase() === "cancel") {
        await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Application cancelled")
          .setDescription(`All good, I've scrapped your ${app.label} application. Nothing got sent.`)] }).catch(() => {});
        return;
      }
      if (!content && msg.attachments?.size) content = [...msg.attachments.values()].map(a => a.url).join("\n"); // image/file-only answer
      answers.push(content ? content.slice(0, cap) : "*(left blank)*");
    }

    // The application could have been closed or deleted mid-interview - re-check before submitting.
    const fresh = getApplication(guild.id, app.key);
    if (!fresh || fresh.closed || !fresh.reviewChannelId) {
      await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Applications closed")
        .setDescription(`Ah, ${app.label} applications shut just as you were wrapping up, so this one didn't make it through. Sorry about the timing - catch it next time they open.`)] }).catch(() => {});
      return;
    }

    const ok = await finalizeApplication(guild, user, fresh, answers, startedAt);
    await dm.send({ embeds: [ok
      ? new EmbedBuilder().setColor(APPY_GREEN).setTitle("Application submitted")
          .setDescription("Your application has been submitted.\n\nThe team will give it a read and get back to you right here. Thanks for taking the time, and good luck!")
      : new EmbedBuilder().setColor(APPY_RED).setTitle("Something went wrong")
          .setDescription("Something broke on my end and your application didn't go through. Give a staff member a nudge and they'll get it sorted.")] }).catch(() => {});
  } finally {
    activeDmApps.delete(user.id);
  }
}

// Post a completed application to its review channel. Returns true on success.
async function finalizeApplication(guild, user, app, answers, startedAt) {
  const reviewChannel = guild.channels.cache.get(app.reviewChannelId);
  if (!reviewChannel) return false;

  const member = await guild.members.fetch(user.id).catch(() => null);
  const durationSec = Math.max(0, Math.round((Date.now() - (startedAt ?? Date.now())) / 1000));
  const joinedUnix = member?.joinedTimestamp ? Math.floor(member.joinedTimestamp / 1000) : null;
  const submittedUnix = Math.floor(Date.now() / 1000);
  const statsLines = [
    `UserId: \`${user.id}\``,
    `Username: \`${user.username}\``,
    `User: <@${user.id}>`,
    `Duration: \`${durationSec}s\``,
    joinedUnix ? `Joined guild <t:${joinedUnix}:R>` : null,
    `Submitted <t:${submittedUnix}:R>`,
  ].filter(Boolean).join("\n");

  const reviewEmbed = new EmbedBuilder()
    .setColor(APP_PENDING)
    .setTitle(`${user.username}'s '${app.label} Application' Application Submitted`.slice(0, 256))
    .setThumbnail(user.displayAvatarURL?.() ?? null)
    .addFields([
      ...app.questions.map((q, i) => ({
        name: `${i + 1}. ${q}`.slice(0, 256),
        value: (answers[i] || "*(left blank)*").slice(0, 1024),
        inline: false,
      })),
      { name: "Submission stats", value: statsLines.slice(0, 1024), inline: false },
    ])
    .setTimestamp();

  const row1 = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(`app_accept_${app.key}_${user.id}`).setLabel("Accept").setStyle(ButtonStyle.Success),
    new ButtonBuilder().setCustomId(`app_deny_${app.key}_${user.id}`).setLabel("Deny").setStyle(ButtonStyle.Danger),
    new ButtonBuilder().setCustomId(`app_acceptwithreason_${app.key}_${user.id}`).setLabel("Accept with reason").setStyle(ButtonStyle.Success),
  );
  const row2 = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(`app_denywithreason_${app.key}_${user.id}`).setLabel("Deny with reason").setStyle(ButtonStyle.Danger),
  );

  const posted = await reviewChannel.send({ embeds: [reviewEmbed], components: [row1, row2] }).catch(() => null);
  if (!posted) return false;
  secLog(guild, "New Application", `<@${user.id}> just applied for **${app.label}**. It's waiting for a look in <#${reviewChannel.id}>.`, COLORS.info);
  return true;
}

// Parse "app_accept_<key>_<userId>" / "app_deny_<key>_<userId>" → { key, userId }.
function parseReviewCustomId(customId, prefix) {
  const rest = customId.slice(prefix.length);
  const lastUnderscore = rest.lastIndexOf("_");
  return { key: rest.slice(0, lastUnderscore), userId: rest.slice(lastUnderscore + 1) };
}

// Shared accept path for both the plain "Accept" button and the "Accept with
// reason" modal submit - grants roles, repaints the review message green with
// every button retired, then DMs the applicant.
async function performAppAccept(interaction, key, userId, reason, messageId) {
  const { guild, member } = interaction;
  const app = getApplication(guild.id, key);
  if (!app) return interaction.reply({ content: "That application type doesn't exist anymore.", ephemeral: true });

  await interaction.deferUpdate().catch(() => {});
  const applicant = await guild.members.fetch(userId).catch(() => null);

  let grantedCount = 0; const failedRoles = [];
  if (applicant) {
    for (const roleId of app.acceptedRoleIds || []) {
      const role = guild.roles.cache.get(roleId);
      if (!role) { failedRoles.push(`\`${roleId}\` (missing)`); continue; }
      if (!role.editable) { failedRoles.push(`${role.name} (above me)`); continue; }
      const ok = await applicant.roles.add(role, `Application accepted by ${member.user.tag}`).then(() => true).catch(() => false);
      if (ok) grantedCount++; else failedRoles.push(role.name);
    }
  }

  const msg = await interaction.channel.messages.fetch(messageId).catch(() => null);
  if (msg && msg.embeds[0]) {
    const updated = EmbedBuilder.from(msg.embeds[0]).setColor(APPY_GREEN);
    await msg.edit({
      embeds: [updated],
      components: [new ActionRowBuilder().addComponents(
        new ButtonBuilder().setCustomId("app_done_accept").setLabel(`Accepted by ${member.user.username}`.slice(0, 80)).setEmoji("✅").setStyle(ButtonStyle.Success).setDisabled(true))],
    }).catch(() => {});
  }

  if (applicant) await applicant.user.send({ embeds: [new EmbedBuilder().setColor(APPY_GREEN).setTitle("Application accepted")
    .setDescription(`Your application for \`${app.label} Application\` has been accepted by <@${member.id}>.${reason ? `\n\nReason: ${reason}` : ""}`)] }).catch(() => {});
  secLog(guild, "Application Accepted",
    `<@${member.id}> accepted <@${userId}>'s **${app.label}** application and handed them **${grantedCount}** role${grantedCount === 1 ? "" : "s"}.` +
    (reason ? `\nReason given: ${reason}` : "") +
    (failedRoles.length ? `\nHeads up, I couldn't grant: ${failedRoles.join(", ")}` : "") +
    (!applicant ? `\nThey've since left the server, so no roles were applied.` : ""),
    COLORS.success);
}

// Shared deny path for both the plain "Deny" button and the "Deny with
// reason" modal submit - repaints the review message red with every button
// retired, then DMs the applicant.
async function performAppDeny(interaction, key, userId, reason, messageId) {
  const { guild, member } = interaction;
  const app = getApplication(guild.id, key);

  await interaction.deferUpdate().catch(() => {});
  const msg = await interaction.channel.messages.fetch(messageId).catch(() => null);
  if (msg && msg.embeds[0]) {
    const updated = EmbedBuilder.from(msg.embeds[0]).setColor(APPY_RED);
    if (reason) updated.addFields({ name: "Reason", value: reason.slice(0, 1024), inline: false });
    await msg.edit({
      embeds: [updated],
      components: [new ActionRowBuilder().addComponents(
        new ButtonBuilder().setCustomId("app_done_deny").setLabel(`Denied by ${member.user.username}`.slice(0, 80)).setEmoji("⛔").setStyle(ButtonStyle.Danger).setDisabled(true))],
    }).catch(() => {});
  }

  const applicant = await guild.members.fetch(userId).catch(() => null);
  if (applicant) await applicant.user.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Application denied")
    .setDescription(`Your application for \`${app?.label || "that role"} Application\` has been denied by <@${member.id}>.${reason ? `\n\nReason: ${reason}` : ""}`)] }).catch(() => {});
  secLog(guild, "Application Denied", `<@${member.id}> turned down <@${userId}>'s **${app?.label || key}** application.${reason ? ` Reason given: ${reason}` : ""}`, COLORS.danger);
}

// "Accept" - immediate, no reason prompt.
async function handleAppAccept(interaction) {
  const { member } = interaction;
  if (!isMod(member)) return interaction.reply({ content: "Only staff can review applications.", ephemeral: true });
  const { key, userId } = parseReviewCustomId(interaction.customId, "app_accept_");
  return performAppAccept(interaction, key, userId, null, interaction.message.id);
}

// "Accept with reason" - opens a modal, actual grant happens on submit.
async function handleAppAcceptWithReason(interaction) {
  const { guild, member } = interaction;
  if (!isMod(member)) return interaction.reply({ content: "Only staff can review applications.", ephemeral: true });
  const { key, userId } = parseReviewCustomId(interaction.customId, "app_acceptwithreason_");
  const app = getApplication(guild.id, key);
  if (!app) return interaction.reply({ content: "That application type doesn't exist anymore.", ephemeral: true });

  const modal = new ModalBuilder().setCustomId(`app_acceptreason_${key}_${userId}_${interaction.message.id}`).setTitle(`Accept ${app.label} Application`.slice(0, 45));
  modal.addComponents(new ActionRowBuilder().addComponents(
    new TextInputBuilder().setCustomId("reason").setLabel("Reason (optional, shared with them)").setStyle(TextInputStyle.Paragraph).setRequired(false).setMaxLength(500)));
  return interaction.showModal(modal);
}

async function handleAppAcceptReasonSubmit(interaction) {
  // customId: app_acceptreason_<key>_<userId>_<messageId>
  const rest = interaction.customId.slice("app_acceptreason_".length);
  const parts = rest.split("_");
  const messageId = parts.pop();
  const userId = parts.pop();
  const key = parts.join("_");
  const reason = interaction.fields.getTextInputValue("reason")?.trim();
  return performAppAccept(interaction, key, userId, reason || null, messageId);
}

// "Deny" - immediate, no reason prompt.
async function handleAppDeny(interaction) {
  const { member } = interaction;
  if (!isMod(member)) return interaction.reply({ content: "Only staff can review applications.", ephemeral: true });
  const { key, userId } = parseReviewCustomId(interaction.customId, "app_deny_");
  return performAppDeny(interaction, key, userId, null, interaction.message.id);
}

// "Deny with reason" - opens a modal, actual denial happens on submit.
async function handleAppDenyWithReason(interaction) {
  const { guild, member } = interaction;
  if (!isMod(member)) return interaction.reply({ content: "Only staff can review applications.", ephemeral: true });
  const { key, userId } = parseReviewCustomId(interaction.customId, "app_denywithreason_");
  const app = getApplication(guild.id, key);
  if (!app) return interaction.reply({ content: "That application type doesn't exist anymore.", ephemeral: true });

  const modal = new ModalBuilder().setCustomId(`app_denyreason_${key}_${userId}_${interaction.message.id}`).setTitle(`Deny ${app.label} Application`.slice(0, 45));
  modal.addComponents(new ActionRowBuilder().addComponents(
    new TextInputBuilder().setCustomId("reason").setLabel("Reason (optional, shared with them)").setStyle(TextInputStyle.Paragraph).setRequired(false).setMaxLength(500)));
  return interaction.showModal(modal);
}

async function handleAppDenyReason(interaction) {
  // customId: app_denyreason_<key>_<userId>_<messageId>
  const rest = interaction.customId.slice("app_denyreason_".length);
  const parts = rest.split("_");
  const messageId = parts.pop();
  const userId = parts.pop();
  const key = parts.join("_");
  const reason = interaction.fields.getTextInputValue("reason")?.trim();
  return performAppDeny(interaction, key, userId, reason || null, messageId);
}

// ── Police Department Manual ──────────────────────────────────
// A single static embed: the officer guide & procedures reference posted via
// /police manual setup. One long description rather than fields, so it reads
// as one continuous sheet instead of a stack of separate boxes.
const POLICE_MANUAL_COLOR = 0xf59e0b; // orange left bar
function buildPoliceManualEmbed() {
  const divider = "-".repeat(42);
  const section = (title, body) => `${divider}\n**${title}**\n${divider}\n\n${body}`;
  const description = [
    "**DEPARTMENT 📖**\n*__Officer Guide & Procedures__*",
    section("OFFICER CONDUCT 👮",
      "**General Expectations**\n" +
      "• Remain respectful towards civilians, suspects, and fellow officers.\n" +
      "• Do not abuse police equipment, powers, or authority.\n" +
      "• Avoid escalating situations without reason.\n" +
      "• Use common sense in all situations.\n" +
      "• Follow instructions from higher-ranking officers.\n\n" +
      "**Professionalism**\n" +
      "• Speak clearly and respectfully.\n" +
      "• Avoid unnecessary arguments with civilians."),
    section("USE OF FORCE ⚖️",
      "**Force Progression**\n" +
      "Verbal Commands → Non-Lethal Force → Deadly Force\n\n" +
      "**Deadly Force Authorization**\n" +
      "Deadly force may only be used when:\n" +
      "• A suspect presents an immediate threat.\n" +
      "• A suspect is actively using deadly force.\n" +
      "• No reasonable alternative exists."),
    section("TRAFFIC STOPS 🚗",
      "**Initiating a Stop**\n" +
      "• Observe a violation.\n" +
      "• Activate emergency lights.\n" +
      "• Follow until safely stopped.\n\n" +
      "**Conducting a Stop**\n" +
      "• Approach carefully.\n" +
      "• Inform driver of reason.\n" +
      "• Allow explanation.\n" +
      "• Determine warning, citation, or arrest.\n\n" +
      "**Officer Safety**\n" +
      "• Remain aware of passengers.\n" +
      "• Watch for suspicious movements.\n" +
      "• Request backup when necessary."),
    section("VEHICLE PURSUITS 🚔",
      "**When to Pursue**\n" +
      "• Driver refuses to stop.\n" +
      "• Fleeing from serious crime.\n" +
      "• Ongoing threat to public safety.\n\n" +
      "**During a Pursuit**\n" +
      "• Update units continuously.\n" +
      "• Maintain visual contact.\n" +
      "• Avoid unnecessary risks.\n\n" +
      "**Ending a Pursuit**\n" +
      "• Suspect apprehended.\n" +
      "• Suspect incapacitated.\n" +
      "• Suspect lost.\n" +
      "• Danger outweighs necessity."),
    section("FELONY STOPS 🔫",
      "Used for:\n" +
      "• Armed suspects\n" +
      "• Violent offenders\n" +
      "• High-risk vehicles\n\n" +
      "**Procedure**\n" +
      "• Maintain distance.\n" +
      "• Give clear commands.\n" +
      "• Remove occupants one at a time.\n" +
      "• Secure suspects.\n" +
      "• Clear vehicle once detained."),
    section("HOSTAGE SITUATIONS 🏠",
      "**Priorities**\n" +
      "Hostage Safety → Officer Safety → Suspect Apprehension\n\n" +
      "**Procedure**\n" +
      "• Establish perimeter.\n" +
      "• Keep unnecessary personnel away.\n" +
      "• Attempt communication.\n" +
      "• Gather information first.\n\n" +
      "**Use of Force**\n" +
      "Deadly force may be used if the suspect presents an immediate threat to a hostage."),
    section("ACTIVE SHOOTER RESPONSE 🚨",
      "**Response Priorities**\n" +
      "• Locate the shooter.\n" +
      "• Stop the threat.\n" +
      "• Protect civilians.\n" +
      "• Coordinate with responding officers.\n\n" +
      "**Officer Actions**\n" +
      "• Move toward the threat when safe.\n" +
      "• Relay descriptions and locations.\n" +
      "• Work together with units."),
    section("ARREST PROCEDURES 🔗",
      "**Making an Arrest**\n" +
      "• Inform suspect they are under arrest.\n" +
      "• Secure suspect.\n" +
      "• State charges.\n" +
      "• Transport safely.\n\n" +
      "**Searches**\n" +
      "• Arrested suspects\n" +
      "• Vehicles connected to investigations\n" +
      "• Areas where evidence may be located"),
    section("FINAL NOTES 📋",
      "This guide covers the core procedures every officer is expected to know. " +
      "It does not replace training, briefings, or direct orders from a superior, " +
      "and when in doubt, ask before acting. Conduct yourself professionally at all times, " +
      "and remember that civilian safety comes first in every situation."),
  ].join("\n\n");

  return new EmbedBuilder().setColor(POLICE_MANUAL_COLOR).setDescription(description);
}

// ── Slash Command Handler ─────────────────────────────────────
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isChatInputCommand()) return;
  if (!interaction.inGuild())
    return interaction.reply({ content: "You can only use this in a server.", ephemeral: true });
  const { commandName, guild, member } = interaction;

  try {
  switch (commandName) {

    // ── /mute ──────────────────────────────────────────────
    case "mute": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target  = interaction.options.getMember("user");
      const minutes = interaction.options.getInteger("minutes") ?? 10;
      const reason  = interaction.options.getString("reason") ?? "No reason provided";
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });
      const muteRoleId = gc(guild).muteRoleId;
      if (!muteRoleId || !guild.roles.cache.get(muteRoleId))
        return interaction.reply({ content: "There is no mute role set up yet. Run `/setup quick`, or set one with `/setup roles mute_role:@Role`.", ephemeral: true });

      if (!isWhitelisted(member)) {
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "mute");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("mute", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "mute");
      }
      const ok = await muteUser(target, minutes, reason);
      if (!ok) return interaction.reply({ content: "There is no mute role set up yet. Run `/setup quick`, or set one with `/setup roles mute_role:@Role`.", ephemeral: true });
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "mute");
      const stashed = mutedRoles[guild.id]?.[target.id]?.roles?.length ?? 0;
      const e = new EmbedBuilder().setColor(COLORS.muted).setTitle("🔇 Member Muted")
        .setDescription(`Muted <@${target.id}> for **${minutes > 0 ? minutes + " minutes" : "as long as it takes"}**.\n**Reason:** ${reason}\nI've set aside **${stashed}** role${stashed === 1 ? "" : "s"} and will hand them back on unmute.`)
        .setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("mute", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /unmute ────────────────────────────────────────────
    case "unmute": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target = interaction.options.getMember("user");
      if (!target) return interaction.reply({ content: "I couldn't find that user.", ephemeral: true });
      if (!gc(guild).muteRoleId) return interaction.reply({ content: "There is no mute role set up yet. Run `/setup quick`, or set one with `/setup roles mute_role:@Role`.", ephemeral: true });
      const stashed = mutedRoles[guild.id]?.[target.id]?.roles?.length ?? 0;
      await unmuteUser(guild, target.id, `Manual unmute by ${interaction.user.tag}`);
      return interaction.reply({ embeds: [new EmbedBuilder().setColor(COLORS.success).setTitle("🔊 Member Unmuted")
        .setDescription(`<@${target.id}> is unmuted, and I gave back **${stashed}** stashed role${stashed === 1 ? "" : "s"}.`).setTimestamp()] });
    }

    // ── /kick ──────────────────────────────────────────────
    case "kick": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target = interaction.options.getMember("user");
      const reason = interaction.options.getString("reason") ?? "No reason provided";
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });

      if (!isWhitelisted(member)) {
        if (bump(guild.id, member.id, "kicks", config.nukeKickThreshold)) {
          resetBump(guild.id, member.id, "kicks");
          await interaction.reply({ content: "Hold on - that just tripped the anti-nuke protection.", ephemeral: true });
          return nukeResponse(guild, member, `Issued ${config.nukeKickThreshold}+ kicks via commands in ${config.nukeWindowMs / 1000}s`);
        }
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "kick");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("kick", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "kick");
      }
      await tryDM(target.user, `You've been kicked from **${guild.name}**.\nReason: ${reason}`);
      await target.kick(reason).catch(() => {});
      secLog(guild, "Member Kicked", `<@${member.id}> kicked <@${target.id}> - ${reason}`, COLORS.danger);
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "kick");
      const e = new EmbedBuilder().setColor(COLORS.danger).setTitle("👢 Member Kicked")
        .setDescription(`Kicked <@${target.id}>.\n**Reason:** ${reason}`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("kick", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /ban ───────────────────────────────────────────────
    case "ban": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target     = interaction.options.getMember("user");
      const reason     = interaction.options.getString("reason") ?? "No reason provided";
      const deleteDays = interaction.options.getInteger("delete_days") ?? 0;
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });

      if (!isWhitelisted(member)) {
        if (bump(guild.id, member.id, "bans", config.nukeBanThreshold)) {
          resetBump(guild.id, member.id, "bans");
          await interaction.reply({ content: "Hold on - that just tripped the anti-nuke protection.", ephemeral: true });
          return nukeResponse(guild, member, `Issued ${config.nukeBanThreshold}+ bans via commands in ${config.nukeWindowMs / 1000}s`);
        }
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "ban");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("ban", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "ban");
      }
      await tryDM(target.user, `You've been banned from **${guild.name}**.\nReason: ${reason}`);
      await target.ban({ reason, deleteMessageSeconds: deleteDays * 86400 }).catch(() => {});
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "ban");
      secLog(guild, "Member Banned", `<@${member.id}> banned <@${target.id}> - ${reason}`, COLORS.danger);
      const e = new EmbedBuilder().setColor(COLORS.danger).setTitle("🔨 Member Banned")
        .setDescription(`Banned <@${target.id}>.\n**Reason:** ${reason}`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("ban", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /unban ─────────────────────────────────────────────
    case "unban": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const userId = interaction.options.getString("user_id").trim();
      const reason = interaction.options.getString("reason") ?? "No reason provided";
      if (!/^\d{17,20}$/.test(userId)) return interaction.reply({ content: "That doesn't look like a valid user ID.", ephemeral: true });
      const ban = await guild.bans.fetch(userId).catch(() => null);
      if (!ban) return interaction.reply({ content: "That user isn't banned.", ephemeral: true });
      await guild.bans.remove(userId, `Unban by ${interaction.user.tag}: ${reason}`).catch(() => {});
      secLog(guild, "Member Unbanned", `<@${member.id}> lifted the ban on \`${userId}\` - ${reason}`, COLORS.success);
      return interaction.reply({ embeds: [new EmbedBuilder().setColor(COLORS.success).setTitle("♻️ Member Unbanned")
        .setDescription(`<@${userId}> (\`${userId}\`) is unbanned.\n**Reason:** ${reason}`).setTimestamp()] });
    }

    // ── /purge ─────────────────────────────────────────────
    case "purge": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const count      = interaction.options.getInteger("count");
      const filterUser = interaction.options.getUser("user");

      if (!isWhitelisted(member)) {
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "purge");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("purge", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "purge");
      }
      await interaction.deferReply({ ephemeral: true });
      let messages = await interaction.channel.messages.fetch({ limit: 100 }).catch(() => null);
      if (!messages) return interaction.editReply("I couldn't fetch the messages here to clear them.");
      if (filterUser) messages = messages.filter(m => m.author.id === filterUser.id);
      const toDelete = [...messages.values()].slice(0, count);
      const deleted  = await interaction.channel.bulkDelete(toDelete, true).catch(() => null);
      const n = deleted?.size ?? 0;
      secLog(guild, "Purge", `<@${member.id}> cleared **${n}** message${n === 1 ? "" : "s"} in <#${interaction.channelId}>${filterUser ? ` from <@${filterUser.id}>` : ""}.`, COLORS.warn);
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "purge");
      const e = new EmbedBuilder().setColor(COLORS.warn).setTitle("🗑️ Messages Cleared")
        .setDescription(`Cleared **${n}** message${n === 1 ? "" : "s"}${filterUser ? ` from <@${filterUser.id}>` : ""}.`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("purge", newUsed, limit) });
      return interaction.editReply({ embeds: [e] });
    }

    // ── /lockdown ──────────────────────────────────────────
    case "lockdown": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const action  = interaction.options.getString("action");
      const channel = interaction.options.getChannel("channel") ?? interaction.channel;
      const lock    = action === "lock";

      if (lock && !isWhitelisted(member)) {
        if (bump(guild.id, member.id, "chLock", config.nukeChannelThreshold)) {
          resetBump(guild.id, member.id, "chLock");
          await interaction.reply({ content: "Hold on - that just tripped the anti-nuke protection.", ephemeral: true });
          return nukeResponse(guild, member, `Locked ${config.nukeChannelThreshold}+ channels via commands in ${config.nukeWindowMs / 1000}s`);
        }
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "lockdown");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("lockdown", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "lockdown");
      }
      await channel.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: lock ? false : null }).catch(() => {});
      secLog(guild, lock ? "Channel Locked" : "Channel Unlocked",
        `<@${member.id}> ${lock ? "locked down" : "reopened"} <#${channel.id}>.`, lock ? COLORS.danger : COLORS.success);
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "lockdown");
      const e = new EmbedBuilder().setColor(lock ? COLORS.danger : COLORS.success)
        .setTitle(lock ? "🔒 Channel Locked" : "🔓 Channel Unlocked")
        .setDescription(`<#${channel.id}> is now ${lock ? "locked down - only staff can send messages" : "back open"}.`).setTimestamp();
      if (lock && !isWhitelisted(member)) e.setFooter({ text: usageFooter("lockdown", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /panic (owner only) - toggles: run again to lift ────
    case "panic": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "This one's owner only.", ephemeral: true });
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
        alertOwner(guild, `<@${member.id}> lifted the panic lockdown. **${unlocked}** channels are back open.`, COLORS.success, "Panic Lockdown Lifted");
        return interaction.editReply(`Done - panic lockdown lifted and **${unlocked}** text channels are back open.`);
      }

      let locked = 0;
      for (const ch of guild.channels.cache.values()) {
        if (ch.isTextBased() && !ch.isThread()) {
          const ok = await ch.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: false }).then(() => true).catch(() => false);
          if (ok) locked++;
        }
      }
      setLockdown(guild.id, "panic", null);
      alertOwner(guild, `<@${member.id}> hit the panic button and locked down **${locked}** channels. Run \`/panic\` again to lift it.`, COLORS.nuke, "Panic Lockdown");
      return interaction.editReply(`Panic lockdown is on - I've locked **${locked}** text channels. Run \`/panic\` again to lift it.`);
    }

    // ── /warn ──────────────────────────────────────────────
    case "warn": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
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
      await tryDM(target.user, `You've picked up a warning in **${guild.name}** (that's #${total}). Reason: ${reason}`);
      secLog(guild, "Warning Issued", `<@${member.id}> warned <@${target.id}> - that's **${total}** now. Reason: ${reason}`, COLORS.warn);

      // Escalation
      let escalation = "";
      if (config.warnBanAt && total >= config.warnBanAt) {
        await target.ban({ reason: `Auto-escalation: reached ${total} warnings` }).catch(() => {});
        escalation = `\n🔨 That hit **${total}** warnings, so they've been auto-banned.`;
        secLog(guild, "Auto-Escalation", `<@${target.id}> hit ${total} warnings and was auto-banned.`, COLORS.danger);
      } else if (config.warnKickAt && total >= config.warnKickAt) {
        await target.kick(`Auto-escalation: reached ${total} warnings`).catch(() => {});
        escalation = `\n👢 That hit **${total}** warnings, so they've been auto-kicked.`;
        secLog(guild, "Auto-Escalation", `<@${target.id}> hit ${total} warnings and was auto-kicked.`, COLORS.danger);
      } else if (config.warnMuteAt && total >= config.warnMuteAt) {
        await muteUser(target, config.warnMuteMin, `Auto-escalation: reached ${total} warnings`);
        escalation = `\n🔇 That hit **${total}** warnings, so they've been auto-muted for ${config.warnMuteMin} min.`;
      }

      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "warn");
      const e = new EmbedBuilder().setColor(COLORS.warn).setTitle("⚠️ Warning Issued")
        .setDescription(`Warned <@${target.id}>. **That's ${total} in total.**\n**Reason:** ${reason}${escalation}`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("warn", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /warnings ──────────────────────────────────────────
    case "warnings": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target = interaction.options.getUser("user");
      const list = getWarnings(guild.id, target.id);
      if (!list.length) return interaction.reply({ content: `<@${target.id}> has a clean slate - no warnings.`, ephemeral: true });
      const lines = list.slice(-15).map((w, i) =>
        `**${i + 1}.** ${w.reason} - by <@${w.by}> · <t:${Math.floor(w.at / 1000)}:R>`).join("\n");
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.warn)
        .setTitle(`⚠️ Warnings for ${target.tag}`)
        .setDescription(`**${list.length} in total.**\n\n${lines}`)
        .setFooter({ text: `Auto-actions kick in at: mute@${config.warnMuteAt} · kick@${config.warnKickAt} · ban@${config.warnBanAt}` })
        .setTimestamp()] });
    }

    // ── /clearwarns ────────────────────────────────────────
    case "clearwarns": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target = interaction.options.getUser("user");
      const had = getWarnings(guild.id, target.id).length;
      clearWarnings(guild.id, target.id);
      secLog(guild, "Warnings Cleared", `<@${member.id}> wiped **${had}** warning${had === 1 ? "" : "s"} for <@${target.id}>.`, COLORS.success);
      return interaction.reply({ embeds: [embed(COLORS.success, `Cleared **${had}** warning${had === 1 ? "" : "s"} for <@${target.id}>. Clean slate.`, "Warnings Cleared")], ephemeral: true });
    }

    // ── /limits ────────────────────────────────────────────
    case "limits": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const windowHours = config.modWindowMs / 3600000;
      if (isWhitelisted(member)) {
        return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
          .setTitle("🛡️ Your Mod Limits")
          .setDescription(`You're whitelisted, so none of the rate limits apply to you.`).setTimestamp()] });
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
        return { name: `${emoji} ${label}${warn}`, value: `\`${bar}\` **${used}/${limit}** used (${pct}%) - **${remaining}** remaining`, inline: false };
      });
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
        .setTitle("📊 Your Mod Action Limits")
        .setDescription(`Here's where you're at over the last **${windowHours}h**. These top back up on their own as older actions age out.`)
        .addFields(...fields).setTimestamp()] });
    }

    // ── /antiping ──────────────────────────────────────────
    case "antiping": {
      if (!isOwner(member) && member.id !== guild.ownerId) return interaction.reply({ content: "Only the bot owner or the server owner can change these settings.", ephemeral: true });
      const sub = interaction.options.getSubcommand();
      const a = ap(guild);
      switch (sub) {
        case "status":
          return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder()
            .setColor(a.enabled ? COLORS.success : COLORS.neutral).setTitle("📡 Anti-Ping - Status")
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
          return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info).setTitle("📡 Anti-Ping - Protected")
            .addFields(
              { name: "Users", value: a.protectedUsers.length ? a.protectedUsers.map(id => `<@${id}>`).join("\n") : "None", inline: true },
              { name: "Roles", value: a.protectedRoles.length ? a.protectedRoles.map(id => `<@&${id}>`).join("\n") : "None", inline: true },
            ).setTimestamp()] });
      }
      return;
    }

    // ── /setup ─────────────────────────────────────────────
    case "setup": {
      if (!isOwner(member) && member.id !== guild.ownerId) return interaction.reply({ content: "Only the bot owner or the server owner can change these settings.", ephemeral: true });
      const sub = interaction.options.getSubcommand();

      if (sub === "quick") {
        await interaction.deferReply({ ephemeral: true });
        const modRoleOpt = interaction.options.getRole("mod_role");
        const { created, reused } = await quickSetupGuild(guild, modRoleOpt);
        const e = buildSetupEmbed(guild, []);
        e.setTitle(`🛡️ Guardian quick setup - ${guild.name}`);
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
        if (!changes.length) return interaction.reply({ content: "Give me at least one role to set.", ephemeral: true });
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
        if (!changes.length) return interaction.reply({ content: "Give me at least one channel to set.", ephemeral: true });
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, changes)] });
      }

      if (sub === "whitelist") {
        const action = interaction.options.getString("action");
        const user   = interaction.options.getUser("user");
        const role   = interaction.options.getRole("role");
        if (!user && !role) return interaction.reply({ content: "Give me a user or a role.", ephemeral: true });
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
      if (!isOwner(member) && member.id !== guild.ownerId) return interaction.reply({ content: "Only the bot owner or the server owner can view the config.", ephemeral: true });
      const windowHours = config.modWindowMs / 3600000;
      const gcfg = gc(guild);
      const acfg = ap(guild);
      const cfgEmbed = new EmbedBuilder().setTitle("🛡️ Guardian Bot - Configuration").setColor(COLORS.info)
        .addFields(
          { name: "🔧 Infrastructure", value: "​", inline: false },
          { name: "Owner(s)",     value: [...BOT_OWNER_IDS].map(id => `<@${id}>`).join(", "), inline: true },
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
          { name: `📊 Mod Daily Limits (${windowHours}h - whitelisted exempt)`, value: "​", inline: false },
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
        return interaction.reply({ content: "This one's owner only.", ephemeral: true });
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

    // ── /status ────────────────────────────────────────────
    case "status": {
      if (!isOwner(interaction.user) && interaction.user.id !== guild.ownerId)
        return interaction.reply({ content: "This one's owner only.", ephemeral: true });
      const mem = process.memoryUsage();
      const lockedCount = Object.keys(lockdownState).length;
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
        .setTitle("📊 Guardian Bot - Status")
        .addFields(
          { name: "Uptime",        value: formatUptime(client.uptime ?? 0), inline: true },
          { name: "WS Ping",       value: `${client.ws.ping}ms`, inline: true },
          { name: "Shard",         value: client.shard ? `${client.shard.ids.join(",")}` : "unsharded", inline: true },
          { name: "Guilds",        value: `${client.guilds.cache.size}`, inline: true },
          { name: "Memory (RSS)",  value: `${Math.round(mem.rss / 1024 / 1024)} MB`, inline: true },
          { name: "Guilds in lockdown", value: `${lockedCount}`, inline: true },
          { name: "Node.js",       value: process.version, inline: true },
        )
        .setFooter({ text: "Use /nuketest to check my permissions in this server." })
        .setTimestamp()] });
    }

    // ── /tickets ───────────────────────────────────────────
    case "tickets": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "Only the bot owner or the server owner can set up tickets.", ephemeral: true });
      const sub = interaction.options.getSubcommand();
      const cfg = getTicketConfig(guild.id);

      if (sub === "addtype") {
        const key = interaction.options.getString("key").trim().toLowerCase().replace(/[^a-z0-9_]/g, "_").slice(0, 32);
        const label = interaction.options.getString("label").trim().slice(0, 80);
        const emoji = interaction.options.getString("emoji").trim();
        const logChannel = interaction.options.getChannel("log_channel");
        if (!key) return interaction.reply({ content: "That key is not valid.", ephemeral: true });
        const types = cfg.types.filter(t => t.key !== key);
        types.push({ key, label, emoji, logChannelId: logChannel.id });
        setTicketConfig(guild.id, { types });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success,
          `Ticket type **${label}** (\`${key}\`) → logs to <#${logChannel.id}>.\nRun \`/tickets panel\` to refresh the panel with this type.`, "Ticket Type Saved")] });
      }
      if (sub === "removetype") {
        const key = interaction.options.getString("key").trim().toLowerCase();
        const had = cfg.types.some(t => t.key === key);
        setTicketConfig(guild.id, { types: cfg.types.filter(t => t.key !== key) });
        return interaction.reply({ ephemeral: true, embeds: [embed(had ? COLORS.success : COLORS.warn,
          had ? `Removed ticket type \`${key}\`. Run \`/tickets panel\` to refresh the panel.` : `No ticket type \`${key}\` was configured.`, "Ticket Type Removed")] });
      }
      if (sub === "listtypes") {
        if (!cfg.types.length) return interaction.reply({ content: "No ticket types configured yet. Use `/tickets addtype`.", ephemeral: true });
        const lines = cfg.types.map(t => `${t.emoji || "🎫"} **${t.label}** (\`${t.key}\`) → <#${t.logChannelId}>`).join("\n");
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, lines, "Ticket Types")] });
      }
      if (sub === "category") {
        const category = interaction.options.getChannel("category");
        setTicketConfig(guild.id, { categoryId: category.id });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `New tickets will be created under **${category.name}**.`, "Ticket Category Set")] });
      }
      if (sub === "panel") {
        if (!cfg.types.length) return interaction.reply({ content: "Set up at least one ticket type first with `/tickets addtype`.", ephemeral: true });
        const channel = interaction.options.getChannel("channel") || (cfg.panelChannelId ? guild.channels.cache.get(cfg.panelChannelId) : null);
        if (!channel) return interaction.reply({ content: "Pick a channel - there is not one set yet.", ephemeral: true });
        await interaction.deferReply({ ephemeral: true });
        const panelEmbed = buildTicketPanelEmbed(guild, cfg);
        const rows = buildTicketPanelRows(cfg);
        let posted = null;
        if (cfg.panelChannelId === channel.id && cfg.panelMessageId) {
          const existingMsg = await channel.messages.fetch(cfg.panelMessageId).catch(() => null);
          if (existingMsg) posted = await existingMsg.edit({ embeds: [panelEmbed], components: rows }).catch(() => null);
        }
        if (!posted) posted = await channel.send({ embeds: [panelEmbed], components: rows }).catch(() => null);
        if (!posted) return interaction.editReply("I could not post the panel there. Please check my permissions in that channel.");
        setTicketConfig(guild.id, { panelChannelId: channel.id, panelMessageId: posted.id });
        return interaction.editReply(`Done - the ticket panel is up in <#${channel.id}>.`);
      }
      return;
    }

    // ── /applications ──────────────────────────────────────
    case "applications": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "Only the bot owner or the server owner can set up applications.", ephemeral: true });
      const sub = interaction.options.getSubcommand();

      if (sub === "list") {
        const apps = Object.values(getApplications(guild.id));
        if (!apps.length) return interaction.reply({ content: "No applications configured. They're seeded automatically when `GUILD_ID` is set.", ephemeral: true });
        const e = new EmbedBuilder().setColor(COLORS.info).setTitle("📝 Applications").setTimestamp();
        for (const a of apps) {
          e.addFields({
            name: `${a.emoji || "📝"} ${a.label} (\`${a.key}\`) - ${a.closed ? "🔒 Closed" : "🟢 Open"}`,
            value: `Panel: ${a.panelChannelId ? `<#${a.panelChannelId}>` : "❌ not set"} · Review: ${a.reviewChannelId ? `<#${a.reviewChannelId}>` : "❌ not set"}\n` +
                   `Roles on accept: ${a.acceptedRoleIds?.length ? a.acceptedRoleIds.map(id => `<@&${id}>`).join(", ") : "none"}\n` +
                   `Questions: ${a.questions?.length || 0}`,
            inline: false,
          });
        }
        return interaction.reply({ ephemeral: true, embeds: [e] });
      }

      // open / close accept a key OR the literal "all" - handle before single-app resolution.
      if (sub === "open" || sub === "close") {
        const wantClosed = sub === "close";
        const rawKey = interaction.options.getString("key")?.trim().toLowerCase();
        await interaction.deferReply({ ephemeral: true });
        const targets = rawKey === "all"
          ? Object.values(getApplications(guild.id))
          : (getApplication(guild.id, rawKey) ? [getApplication(guild.id, rawKey)] : []);
        if (!targets.length)
          return interaction.editReply(`There is no application with the key \`${rawKey}\`. Run \`/applications list\` to see the valid keys (or use \`all\`).`);
        const changed = [];
        for (const a of targets) {
          setApplication(guild.id, a.key, { closed: wantClosed });
          await refreshAppPanel(guild, getApplication(guild.id, a.key)).catch(() => {});
          changed.push(a.label);
        }
        secLog(guild, wantClosed ? "Applications Closed" : "Applications Opened",
          `<@${member.id}> ${wantClosed ? "closed" : "opened"} application(s): ${changed.join(", ")}`, wantClosed ? COLORS.neutral : COLORS.success);
        return interaction.editReply({ embeds: [embed(wantClosed ? COLORS.neutral : COLORS.success,
          `${wantClosed ? "🔒 Closed" : "🟢 Opened"} **${changed.length}** application(s): ${changed.join(", ")}.\nThe panel button${changed.length === 1 ? " has" : "s have"} been updated.`, "Applications")] });
      }

      const key = interaction.options.getString("key")?.trim().toLowerCase();
      const app = getApplication(guild.id, key);
      if (!app) return interaction.reply({ content: `There is no application with the key \`${key}\`. Run \`/applications list\` to see the valid keys.`, ephemeral: true });

      if (sub === "panel") {
        const channelOpt = interaction.options.getChannel("channel");
        await interaction.deferReply({ ephemeral: true });
        // Moving to a new channel? Re-home this app and drop its old panel message id.
        if (channelOpt && channelOpt.id !== app.panelChannelId) setApplication(guild.id, key, { panelChannelId: channelOpt.id, panelMessageId: "" });
        const channelId = channelOpt?.id || app.panelChannelId;
        if (!channelId) return interaction.editReply("Pick a channel - there is not one set for this application yet.");
        const channel = guild.channels.cache.get(channelId);
        if (!channel) return interaction.editReply("I cannot find that channel.");
        // Render the whole channel group, so a shared channel (e.g. Gambino +
        // Colombo) posts one combined panel rather than one per app.
        const apps = appsByPanelChannel(guild.id).get(channelId) || [getApplication(guild.id, key)];
        await renderChannelPanel(guild, channelId, apps);
        return interaction.editReply(`Done - the application panel (${apps.map(a => a.label).join(", ")}) is up in <#${channelId}>.`);
      }
      if (sub === "setreview") {
        const channel = interaction.options.getChannel("channel");
        setApplication(guild.id, key, { reviewChannelId: channel.id });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `**${app.label}** applications will be sent to <#${channel.id}> for review.`, "Applications")] });
      }
      if (sub === "setpanelchannel") {
        const channel = interaction.options.getChannel("channel");
        setApplication(guild.id, key, { panelChannelId: channel.id, panelMessageId: "" });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `**${app.label}** panel channel set to <#${channel.id}>. Run \`/applications panel key:${key}\` to post it.`, "Applications")] });
      }
      if (sub === "addrole") {
        const role = interaction.options.getRole("role");
        const roles = [...(app.acceptedRoleIds || [])];
        if (!roles.includes(role.id)) roles.push(role.id);
        setApplication(guild.id, key, { acceptedRoleIds: roles });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `<@&${role.id}> will be granted when a **${app.label}** application is accepted.`, "Applications")] });
      }
      if (sub === "removerole") {
        const role = interaction.options.getRole("role");
        setApplication(guild.id, key, { acceptedRoleIds: (app.acceptedRoleIds || []).filter(id => id !== role.id) });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `<@&${role.id}> removed from **${app.label}** accepted-roles.`, "Applications")] });
      }
      if (sub === "setquestions") {
        const questions = interaction.options.getString("questions").split("|").map(q => q.trim()).filter(Boolean);
        if (!questions.length) return interaction.reply({ content: "Give at least one question, separated by `|`.", ephemeral: true });
        setApplication(guild.id, key, { questions });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success,
          `**${app.label}** now has **${questions.length}** question(s):\n${questions.map((q, i) => `${i + 1}. ${q}`).join("\n")}`, "Applications")] });
      }
      return;
    }

    // ── /police manual setup ────────────────────────────────
    case "police": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "Only the bot owner or the server owner can set up the police manual.", ephemeral: true });
      const group = interaction.options.getSubcommandGroup();
      const sub = interaction.options.getSubcommand();
      if (group === "manual" && sub === "setup") {
        const channel = interaction.options.getChannel("channel") || interaction.channel;
        await interaction.deferReply({ ephemeral: true });
        const posted = await channel.send({ embeds: [buildPoliceManualEmbed()] }).catch(() => null);
        if (!posted) return interaction.editReply("I couldn't post there. Check that I have permission to send messages and embeds in that channel.");
        return interaction.editReply(`Done - the officer guide & procedures manual is up in <#${channel.id}>.`);
      }
      return;
    }

    // ── /help ──────────────────────────────────────────────
    case "help": {
      const windowHours = config.modWindowMs / 3600000;
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
        .setTitle("🛡️ Guardian Bot - Commands")
        .addFields(
          { name: "🔇 /mute", value: "`@user [minutes] [reason]` - Mute (roles stashed & restored on unmute)", inline: false },
          { name: "🔊 /unmute", value: "`@user` - Unmute & restore stashed roles", inline: false },
          { name: "👢 /kick", value: "`@user [reason]` - Kick a member", inline: false },
          { name: "🔨 /ban", value: "`@user [reason] [delete_days]` - Ban a member", inline: false },
          { name: "♻️ /unban", value: "`user_id [reason]` - Unban by ID", inline: false },
          { name: "🗑️ /purge", value: "`count [user]` - Bulk-delete messages", inline: false },
          { name: "🔒 /lockdown", value: "`lock|unlock [channel]` - Lock or unlock a channel", inline: false },
          { name: "🚨 /panic", value: "Emergency lock **all** text channels *(owner only)*", inline: false },
          { name: "⚠️ /warn", value: "`@user [reason]` - Warn (auto-escalates to mute/kick/ban)", inline: false },
          { name: "📋 /warnings", value: "`@user` - View a member's warnings", inline: false },
          { name: "🧹 /clearwarns", value: "`@user` - Clear a member's warnings", inline: false },
          { name: "📡 /antiping", value: "Configure ping protection - `status`, `toggle`, `action`, `protect`, etc. *(bot owner only)*", inline: false },
          { name: "📊 /limits", value: "Check your remaining mod action limits today", inline: false },
          { name: "⚙️ /config", value: "View configuration *(bot owner only)*", inline: false },
          { name: "🔧 /setup", value: "`quick` auto-provisions a mute role + log channels in one step; `view`/`roles`/`channels`/`whitelist`/`failsafe` configure individual fields *(bot/server owner only)*", inline: false },
          { name: "🎫 /tickets", value: "`addtype`/`removetype`/`listtypes`/`category`/`panel` - configure the ticket system *(bot/server owner only)*", inline: false },
          { name: "📝 /applications", value: "`open`/`close` (accepts a key or `all`), `list`/`panel`/`setreview`/`setpanelchannel`/`addrole`/`removerole`/`setquestions` - configure the application system *(bot/server owner only)*", inline: false },
          { name: "👮 /police", value: "`manual setup [channel]` - post the officer guide & procedures manual *(bot/server owner only)*", inline: false },
          { name: "🧪 /nuketest", value: "Confirm anti-nuke + check my permissions *(owner only)*", inline: false },
          { name: "📈 /status", value: "Bot health: uptime, latency, guild count, memory *(owner only)*", inline: false },
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

// ── Ticket buttons (panel + in-ticket controls) ────────────────
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isButton() || !interaction.inGuild()) return;
  try {
    if (interaction.customId.startsWith("ticket_open_")) return await handleTicketOpen(interaction);
    if (interaction.customId === "ticket_claim") return await handleTicketClaim(interaction);
    if (interaction.customId === "ticket_close") return await handleTicketClose(interaction);
  } catch (err) {
    console.error("⚠️ ticket button handler failed:", err);
    const msg = { content: "⚠️ Something went wrong.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});

// ── Ticket "reason" modal submit → actually creates the channel ──
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isModalSubmit() || !interaction.inGuild()) return;
  if (!interaction.customId.startsWith("ticket_reason_")) return;
  try {
    const key = interaction.customId.replace("ticket_reason_", "");
    const reason = interaction.fields.getTextInputValue("reason");
    await createTicketChannel(interaction, key, reason);
  } catch (err) {
    console.error("⚠️ ticket modal handler failed:", err);
    const msg = { content: "⚠️ Something went wrong opening your ticket.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});

// ── Application buttons (apply / accept / deny) ────────────────
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isButton() || !interaction.inGuild()) return;
  try {
    if (interaction.customId.startsWith("app_apply_"))  return await handleAppApply(interaction);
    if (interaction.customId.startsWith("app_acceptwithreason_")) return await handleAppAcceptWithReason(interaction);
    if (interaction.customId.startsWith("app_accept_")) return await handleAppAccept(interaction);
    if (interaction.customId.startsWith("app_denywithreason_")) return await handleAppDenyWithReason(interaction);
    if (interaction.customId.startsWith("app_deny_"))   return await handleAppDeny(interaction);
  } catch (err) {
    console.error("⚠️ application button handler failed:", err);
    const msg = { content: "⚠️ Something went wrong.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});

// ── Application accept/deny-reason modal submits (staff review only) ───
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isModalSubmit() || !interaction.inGuild()) return;
  const isAccept = interaction.customId.startsWith("app_acceptreason_");
  const isDeny = interaction.customId.startsWith("app_denyreason_");
  if (!isAccept && !isDeny) return;
  try {
    if (isAccept) await handleAppAcceptReasonSubmit(interaction);
    else await handleAppDenyReason(interaction);
  } catch (err) {
    console.error("⚠️ application modal handler failed:", err);
    const msg = { content: "⚠️ Something went wrong.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});

// ── Boot ──────────────────────────────────────────────────────
client.once(Events.ClientReady, async () => {
  console.log(`✅ Guardian Bot online as ${client.user.tag}`);
  console.log(`👑 Owner(s): ${[...BOT_OWNER_IDS].join(", ")}`);
  client.user.setActivity("Protecting the server 🛡️", { type: ActivityType.Watching });
  if (!client.shard || client.shard.ids.includes(0)) await registerCommandsGlobal();
  await clearStaleGuildCommands(); // per-shard: only this shard's own cached guilds
  await recoverMutes();
  await recoverLockdowns();

  // Post any configured ticket + application panels that aren't already up (idempotent).
  for (const guild of client.guilds.cache.values()) {
    try { await ensureTicketPanel(guild); } catch (_) {}
    try { await ensureApplicationPanels(guild); } catch (_) {}
  }

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
    try { const r = await snapshotGuild(guild); console.log(`📸 [${guild.name}] snapshot: ${r.roles} roles, ${r.channels} channels`); } catch (_) {}
  }
  const snapTimer = setInterval(async () => {
    for (const guild of client.guilds.cache.values()) { try { await snapshotGuild(guild); } catch (_) {} }
  }, config.snapshotIntervalMs);
  if (snapTimer.unref) snapTimer.unref();
});

client.on("error", e => console.error("client error:", e));
process.on("unhandledRejection", e => console.error("unhandledRejection:", e));

// When added to a new server: snapshot it and notify owner. (Global commands
// already cover new guilds automatically - no per-guild registration needed.)
client.on(Events.GuildCreate, async (guild) => {
  console.log(`➕ Joined guild ${guild.name} (${guild.id})`);
  try { await snapshotGuild(guild); } catch (_) {}
  try { await ensureTicketPanel(guild); } catch (_) {}
  try { await ensureApplicationPanels(guild); } catch (_) {}
  // Clear any stray guild-scoped commands (e.g. from earlier per-guild testing on
  // this server before Guardian was invited) so nothing duplicates the global set.
  try {
    const rest = new REST({ version: "10" }).setToken(TOKEN);
    const existing = await rest.get(Routes.applicationGuildCommands(CLIENT_ID, guild.id));
    if (existing.length) {
      await rest.put(Routes.applicationGuildCommands(CLIENT_ID, guild.id), { body: [] });
      console.log(`🧹 Cleared ${existing.length} stale guild-scoped command(s) in ${guild.name}`);
    }
  } catch (_) {}
  if (config.ownerDM)
    for (const id of BOT_OWNER_IDS)
      client.users.fetch(id)
        .then(u => u.send(`Just got added to **${guild.name}** (\`${guild.id}\`). To get set up fast, run \`/setup quick\` over there - it'll create a mute role and the log channels for you. Then point me at your staff role with \`/setup roles mod_role:@YourStaffRole\` and you're good.`))
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
      alertOwner(guild, "I've lost some permissions I really need (View Audit Log, Ban Members, or Manage Roles), which means anti-nuke could be flying blind right now. Please check my role position and permissions as soon as you can.", COLORS.danger, "I Need My Permissions Back");
    healthState.set(guild.id, ok);
  }
}, 60000);
if (sweep.unref) sweep.unref();

// Graceful shutdown: flush the DB (WAL) and disconnect cleanly.
for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => {
    console.log(`\n${sig} received - shutting down…`);
    try { db.close(); } catch (_) {}
    try { client.destroy(); } catch (_) {}
    process.exit(0);
  });
}

// Only actually connect to Discord when run directly (`node index.js` / `npm start`),
// not when required by the test suite (`require("../index.js")`).
if (require.main === module) client.login(TOKEN);

// ── Exports (for the test suite - node:test in test/*.test.js) ─────────────
// Deliberately limited to pure/state-only logic that doesn't need a live
// Discord connection: config merging, rate limits, lockdown state, warn
// escalation math, embed formatting helpers. Discord-event handlers and
// anything that touches the gateway are exercised by hand against a real
// bot instead - there's no practical way to unit-test those without it.
module.exports = {
  gc, setGuild, ap, setAntiPing,
  isOwner, BOT_OWNER_IDS,
  checkModLimit, recordModAction, pruneWindow,
  bump, resetBump, bumpStorm,
  isLockdown, setLockdown, clearLockdown,
  buildBar, usageFooter, renderAntiPingResponse,
  canActOn,
  getTicketConfig, setTicketConfig,
  getOpenTicket, setOpenTicket, deleteOpenTicket, findOpenTicketByUser,
  getApplications, getApplication, setApplication,
};
