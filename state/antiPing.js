// ── Anti-Ping runtime state (persisted to antiping.json) ──────
const { dbLoadAll, dbPut, importJsonIfPresent } = require("../lib/db");
const { config, ANTIPING_FILE } = require("../lib/config");

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

module.exports = { antiPingDefaults, ap, setAntiPing };
