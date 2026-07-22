// ── Muted-role stash state (persisted to mutedroles.json) ─────
// Shape: { [guildId]: { [userId]: { roles:[ids], reason, mutedAt, expiresAt|null } } }
const { dbLoadAll, dbPut, importJsonIfPresent } = require("../lib/db");
const { MUTED_FILE } = require("../lib/config");

let mutedRoles = {};
function loadMutedRoles() { importJsonIfPresent("muted_roles", MUTED_FILE); mutedRoles = dbLoadAll("muted_roles"); }
function saveMutedRoles(gid) { dbPut("muted_roles", gid, mutedRoles[gid]); }
loadMutedRoles();

module.exports = { mutedRoles, saveMutedRoles };
