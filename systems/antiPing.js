// ── Anti-Ping ─────────────────────────────────────────────────
const { ap } = require("../state/antiPing");
const { isMod, isWhitelisted } = require("../lib/permissions");
const { COLORS, embed, secLog, renderAntiPingResponse } = require("../lib/embeds");
const { muteUser } = require("./mute");

async function checkAntiPing(message) {
  const a = ap(message.guild);
  if (!a.enabled) return;
  const member = message.member;
  if (!member) return;
  if (member.id === message.guild.ownerId) return;
  if (isMod(member) || isWhitelisted(member)) return;

  const hits = new Set();
  for (const [id, user] of message.mentions.users) {
    if (id === message.author.id || user.bot) continue;
    if (a.ignoreReplies && message.mentions.repliedUser?.id === id) continue;
    if (a.protectedUsers.includes(id)) { hits.add(`<@${id}>`); continue; }
    const t = message.guild.members.cache.get(id);
    if (t && t.roles.cache.some(r => a.protectedRoles.includes(r.id))) hits.add(`<@${id}>`);
  }
  for (const [id] of message.mentions.roles) {
    if (a.protectedRoles.includes(id)) hits.add(`<@&${id}>`);
  }
  if (hits.size === 0) return;

  const targets = [...hits].join(", ");
  const reason  = `Anti-ping: mentioned protected ${targets}`;
  if (a.deleteMessage) message.delete().catch(() => {});

  let actionText = "logged only";
  switch (a.action) {
    case "mute":    await muteUser(member, a.timeoutMin, reason); actionText = `muted for ${a.timeoutMin} min`; break;
    case "timeout": await member.timeout(a.timeoutMin * 60000, reason).catch(() => {}); actionText = `timed out for ${a.timeoutMin} min`; break;
    case "warn":    actionText = "warned"; break;
  }
  if (a.notifyChannel) {
    message.channel.send({ embeds: [embed(COLORS.warn, renderAntiPingResponse(a, member.id, targets, actionText), "Anti-Ping")] })
      .then(m => setTimeout(() => m.delete().catch(() => {}), 8000)).catch(() => {});
  }
  secLog(message.guild, "📡 Anti-Ping Triggered",
    `<@${member.id}> pinged ${targets} in <#${message.channel.id}>, so they were **${actionText}**.`, COLORS.warn);
}

module.exports = { checkAntiPing };
