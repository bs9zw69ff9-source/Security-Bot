// ── Mute Utility (strips + stashes roles, restores on unmute) ─
const { gc } = require("../state/guildSettings");
const { mutedRoles, saveMutedRoles } = require("../state/mutedRoles");
const { lockdownState, liftLockdownChannels } = require("../state/lockdown");
const { COLORS, secLog, scheduleTask } = require("../lib/embeds");
const client = require("../lib/client");

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

module.exports = { muteUser, unmuteUser, recoverMutes, recoverLockdowns };
