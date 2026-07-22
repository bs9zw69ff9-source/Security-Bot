// ── Anti-Nuke engine (scoped per guild - a user's actions in one server never
//    count toward thresholds in another) ───────────────────────────────────
const { Events, AuditLogEvent, PermissionsBitField } = require("discord.js");
const client = require("../lib/client");
const { config, DANGER_PERMS } = require("../lib/config");
const { isWhitelisted } = require("../lib/permissions");
const { COLORS, secLog, alertOwner } = require("../lib/embeds");
const { setLockdown } = require("../state/lockdown");

const nukeTracker = new Map();     // "gid:uid" -> dynamic action arrays

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

module.exports = {
  nukeTracker, nukeStormTracker,
  getNukeEntry, pruneOld, bump, resetBump, bumpStorm,
  nukeResponse, serverEmergencyLock,
};
