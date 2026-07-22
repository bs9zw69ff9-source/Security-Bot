// ── Embed Helpers ─────────────────────────────────────────────
const { EmbedBuilder } = require("discord.js");
const { appendForensic } = require("./db");
const { config, BOT_OWNER_IDS } = require("./config");
const client = require("./client");
const { gc } = require("../state/guildSettings");

const COLORS = {
  success: 0x00e5a0, warn: 0xf5a623, danger: 0xff3b5c, info: 0x5865f2,
  muted: 0xff7518, nuke: 0xff0033, neutral: 0x2f3136,
};
// Appy-style accent colours for the application DM flow and review embed.
const APPY_GREEN   = 0x57f287; // intro / submitted / accepted (green left bar)
const APPY_BLURPLE = 0x5865f2; // per-question prompts (blurple left bar)
const APPY_RED     = 0xed4245; // denied (red left bar)
const APP_PENDING  = 0xf59e0b; // review pending (orange left bar)

function embed(color, description, title = null) {
  const e = new EmbedBuilder().setColor(color).setDescription(description).setTimestamp();
  if (title) e.setTitle(`🛡️ ${title}`);
  return e;
}

async function secLog(guild, title, desc, color = COLORS.success) {
  appendForensic(guild.id, "log", { title, desc });
  const logId = gc(guild).logChannelId;
  if (!logId) return;
  const ch = guild.channels.cache.get(logId);
  if (!ch) return;
  ch.send({ embeds: [embed(color, desc, title)] }).catch(() => {});
}

// Critical alert: forensic trail + channel ping + owner DM (so a nuked log channel can't blind the owner).
function alertOwner(guild, desc, color = COLORS.nuke, title = "Security Alert") {
  appendForensic(guild.id, "alert", { title, desc });
  const g = gc(guild);
  const chId = g.alertChannelId || g.logChannelId;
  const ch = chId ? guild.channels.cache.get(chId) : null;
  const ownerIds = [...BOT_OWNER_IDS];
  if (ch) ch.send({
    content: ownerIds.map(id => `<@${id}>`).join(" "),
    embeds: [embed(color, desc, title)],
    allowedMentions: { users: ownerIds },
  }).catch(() => {});
  if (config.ownerDM)
    for (const id of ownerIds)
      client.users.fetch(id)
        .then(u => u.send({ embeds: [embed(color, `**[${guild.name}]** ${desc}`, title)] }))
        .catch(() => {});
}

function buildBar(used, limit, width = 10) {
  const filled = Math.min(width, Math.round((used / Math.max(limit, 1)) * width));
  return "█".repeat(filled) + "░".repeat(width - filled);
}
function usageFooter(action, used, limit) {
  const remaining = limit - used;
  const bar = buildBar(used, limit, 10);
  const warning = remaining <= Math.ceil(limit * 0.2) && remaining > 0
    ? `\nJust **${remaining}** ${action}${remaining === 1 ? "" : "s"} remaining today.` : "";
  return `\`${bar}\` **${used}/${limit}** ${action}s used today${warning}`;
}
function limitDeniedEmbed(action, used, limit, resetsInMin) {
  return embed(COLORS.danger,
    `You've hit your \`/${action}\` limit for now.\n\n` +
    `That's **${used}/${limit}** ${action}s in the last ${config.modWindowMs / 3600000}h. ` +
    `You'll be able to use it again in about **${resetsInMin} minute${resetsInMin === 1 ? "" : "s"}**.`);
}

function renderAntiPingResponse(a, memberId, targets, actionText) {
  return a.responseTemplate
    .split("{user}").join(`<@${memberId}>`)
    .split("{targets}").join(targets)
    .split("{action}").join(actionText);
}

function formatUptime(ms) {
  const s = Math.floor(ms / 1000);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}d ${h}h ${m}m`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m ${s % 60}s`;
}

// setTimeout overflows (and fires immediately) past ~24.8 days. Chunk long delays.
const MAX_TIMEOUT = 2147483647;
function scheduleTask(fn, delayMs) {
  if (delayMs <= MAX_TIMEOUT) return setTimeout(fn, Math.max(0, delayMs));
  return setTimeout(() => scheduleTask(fn, delayMs - MAX_TIMEOUT), MAX_TIMEOUT);
}

module.exports = {
  COLORS, APPY_GREEN, APPY_BLURPLE, APPY_RED, APP_PENDING,
  embed, secLog, alertOwner,
  buildBar, usageFooter, limitDeniedEmbed, renderAntiPingResponse,
  formatUptime, scheduleTask, MAX_TIMEOUT,
};
