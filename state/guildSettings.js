// ── Per-guild settings (set via /setup; override .env defaults) ──
const { dbLoadAll, dbPut, importJsonIfPresent } = require("../lib/db");
const { config, GUILD_ID, SETTINGS_FILE } = require("../lib/config");

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

module.exports = { gc, setGuild };
