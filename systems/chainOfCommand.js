// ── Chain of Command ──────────────────────────────────────────
// One embed per board, listing each group's roles (in hierarchy order) next
// to whoever currently holds them. Posted once via /chainofcommand setup,
// then kept in sync automatically as members' roles change.
const { Events, EmbedBuilder } = require("discord.js");
const client = require("../lib/client");
const { COLORS } = require("../lib/embeds");
const { getChainKeys, getChain, setChain, getAllChainRoleIds } = require("../state/chainOfCommand");

function buildChainOfCommandEmbed(guild, groups, title) {
  // Discord only resolves @mentions in an embed's description/field VALUE,
  // never in a field NAME - so roles have to live in the description
  // alongside their holders, not as field headers, or they render as raw
  // <@&id> text instead of an actual mention.
  const groupBlocks = [];
  for (const group of groups) {
    const roleBlocks = [];
    for (const roleId of group.roleIds || []) {
      const role = guild.roles.cache.get(roleId);
      if (!role) continue;
      const holders = [...role.members.values()].sort((a, b) => a.user.username.localeCompare(b.user.username));
      roleBlocks.push(`<@&${roleId}>\n${holders.length ? holders.map(m => `<@${m.id}>`).join("\n") : "*(none)*"}`);
    }
    if (!roleBlocks.length) continue;
    groupBlocks.push(group.label ? `**${group.label}**\n${roleBlocks.join("\n\n")}` : roleBlocks.join("\n\n"));
  }
  return new EmbedBuilder().setColor(COLORS.info).setTitle(title || "📋 Chain of Command").setTimestamp()
    .setDescription(groupBlocks.length ? groupBlocks.join("\n\n").slice(0, 4096) : "None of the configured roles exist in this server anymore.");
}

// Post or refresh (edit-in-place) one board for a guild, if configured.
// Safe to call often - a no-op when that key isn't set up yet.
async function renderChainOfCommand(guild, key) {
  const cfg = getChain(guild.id, key);
  if (!cfg.channelId || !cfg.groups.length) return;
  const channel = guild.channels.cache.get(cfg.channelId);
  if (!channel) return;
  // Without a full member fetch, guild.members.cache (and so role.members)
  // only holds whoever the bot has already seen - most holders would be
  // missing on a server this bot hasn't fully cached yet.
  await guild.members.fetch().catch(() => {});
  const payload = { embeds: [buildChainOfCommandEmbed(guild, cfg.groups, cfg.title)] };
  if (cfg.messageId) {
    const existing = await channel.messages.fetch(cfg.messageId).catch(() => null);
    if (existing) { await existing.edit(payload).catch(() => {}); return; }
  }
  const posted = await channel.send(payload).catch(() => null);
  if (posted) setChain(guild.id, key, { messageId: posted.id });
}

// Render every board configured for a guild - used on boot/join and after a
// tracked role change, since either could touch any one of them.
async function renderAllChainsOfCommand(guild) {
  for (const key of getChainKeys(guild.id)) await renderChainOfCommand(guild, key).catch(() => {});
}

// Debounced per-guild refresh so a burst of role changes (e.g. a bulk sync)
// collapses into one re-render instead of one edit per member.
const chainOfCommandRefreshTimers = new Map(); // guildId -> Timeout
function scheduleChainOfCommandRefresh(guild) {
  if (!getChainKeys(guild.id).length) return;
  clearTimeout(chainOfCommandRefreshTimers.get(guild.id));
  const t = setTimeout(() => renderAllChainsOfCommand(guild).catch(() => {}), 3000);
  if (t.unref) t.unref();
  chainOfCommandRefreshTimers.set(guild.id, t);
}

// Chain of command: refresh whenever a tracked role is gained/lost, or a
// holder leaves the server outright.
client.on(Events.GuildMemberUpdate, (oldMember, newMember) => {
  const tracked = getAllChainRoleIds(newMember.guild.id);
  if (!tracked.size) return;
  const touchesTracked = [...tracked].some(id => oldMember.roles.cache.has(id) !== newMember.roles.cache.has(id));
  if (touchesTracked) scheduleChainOfCommandRefresh(newMember.guild);
});
client.on(Events.GuildMemberRemove, (member) => {
  const tracked = getAllChainRoleIds(member.guild.id);
  if ([...tracked].some(id => member.roles?.cache?.has(id))) scheduleChainOfCommandRefresh(member.guild);
});

module.exports = { buildChainOfCommandEmbed, renderChainOfCommand, renderAllChainsOfCommand };
