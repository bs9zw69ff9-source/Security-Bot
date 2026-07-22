// ── Anti-Raid (join velocity + new-account quarantine) ────────
const { Events } = require("discord.js");
const client = require("../lib/client");
const { config } = require("../lib/config");
const { tryDM } = require("../lib/permissions");
const { COLORS, secLog, alertOwner, scheduleTask } = require("../lib/embeds");
const { isLockdown, setLockdown, liftLockdownChannels } = require("../state/lockdown");

const joinTracker = new Map();     // gid -> [timestamps]

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

module.exports = { joinTracker };
