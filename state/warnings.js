// ── Warnings state (persisted to warnings.json) ───────────────
// Shape: { [guildId]: { [userId]: [{ reason, by, at }] } }
const { dbLoadAll, dbPut, importJsonIfPresent } = require("../lib/db");
const { WARN_FILE } = require("../lib/config");

let warnings = {};
function loadWarnings() { importJsonIfPresent("warnings", WARN_FILE); warnings = dbLoadAll("warnings"); }
function saveWarnings(gid) { dbPut("warnings", gid, warnings[gid]); }
loadWarnings();

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

module.exports = { addWarning, getWarnings, clearWarnings };
