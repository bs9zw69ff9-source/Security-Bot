// ── Lockdown state (persisted to SQLite `lockdown_state`) ──────
const { dbLoadAll, dbPut } = require("../lib/db");
const { secLog } = require("../lib/embeds");

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

module.exports = { lockdownState, isLockdown, setLockdown, clearLockdown, liftLockdownChannels };
