// ── Hidden owner-only FAILSAFE (message commands, NOT slash-registered) ──
// Target roles are configured per guild via `/setup failsafe` (gc(guild).failsafeRoleIds) -
// NOT hardcoded, so this works for whatever server the bot is running in, not just one.
const { PermissionsBitField } = require("discord.js");
const { dbLoadAll, dbPut, importJsonIfPresent } = require("../lib/db");
const { FAILSAFE_FILE } = require("../lib/config");
const { COLORS, alertOwner } = require("../lib/embeds");
const { gc } = require("../state/guildSettings");
const client = require("../lib/client");

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

module.exports = { runFailsafe, runRestore };
