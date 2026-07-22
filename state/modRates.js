// ── Mod Rate Limit state (persisted to SQLite `mod_rates`) - scoped + persisted
//    per guild so a mod's limits in one server are independent of, and survive
//    restarts independently of, their activity in any other. ──────────────
const { dbLoadAll, dbPut } = require("../lib/db");
const { config } = require("../lib/config");

let modRates = {}; // { [guildId]: { [userId]: { bans:[], kicks:[], mutes:[], purges:[], lockdowns:[], warns:[] } } }
function loadModRates() { modRates = dbLoadAll("mod_rates"); }
function saveModRates(gid) { dbPut("mod_rates", gid, modRates[gid]); }
loadModRates();

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

module.exports = { getModEntry, pruneWindow, checkModLimit, recordModAction };
