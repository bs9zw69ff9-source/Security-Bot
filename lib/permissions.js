const { BOT_OWNER_IDS } = require("./config");
const { gc } = require("../state/guildSettings");

function isOwner(idOrMember) {
  const id = typeof idOrMember === "string" ? idOrMember : idOrMember?.id;
  return BOT_OWNER_IDS.has(id);
}

function isMod(member) {
  if (!member) return false;
  if (isOwner(member)) return true;
  if (member.id === member.guild.ownerId) return true;
  const modRoleId = gc(member.guild).modRoleId;
  if (!modRoleId) return false;
  return member.roles.cache.has(modRoleId);
}

function isWhitelisted(member) {
  if (!member) return false;
  if (isOwner(member)) return true;                       // hardcoded owner is always immune
  if (member.id === member.guild.ownerId) return true;
  const g = gc(member.guild);
  if (g.nukeWhitelistUserIds.includes(member.id)) return true;
  return member.roles.cache.some(r => g.nukeWhitelistRoleIds.includes(r.id));
}

// Best-effort DM to a member before punitive action.
async function tryDM(user, text) {
  try { await user.send(text); } catch (_) {}
}

// Guard: can `actor` moderate `target`? Protects owner/whitelist and respects hierarchy.
function canActOn(actor, target) {
  if (!target) return { ok: false, why: "I can't find that user in this server." };
  if (isOwner(target)) return { ok: false, why: "That's the bot owner, so they're off-limits." };
  if (target.id === target.guild.ownerId) return { ok: false, why: "That's the server owner - can't touch them." };
  if (isWhitelisted(target)) return { ok: false, why: "That user's whitelisted, so they're protected." };
  if (target.id === actor.id) return { ok: false, why: "You can't do that to yourself." };
  const me = target.guild.members.me;
  if (me && target.roles.highest.position >= me.roles.highest.position)
    return { ok: false, why: "Their top role sits above mine, so I can't. Bump my role higher and try again." };
  const actorPrivileged = isOwner(actor) || actor.id === actor.guild.ownerId;
  if (!actorPrivileged && target.roles.highest.position >= actor.roles.highest.position)
    return { ok: false, why: "Their role is the same as or higher than yours, so this one's out of your reach." };
  return { ok: true };
}

module.exports = { isOwner, isMod, isWhitelisted, tryDM, canActOn };
