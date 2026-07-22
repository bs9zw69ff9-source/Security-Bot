// ── /setup helpers ──────────────────────────────────────────────
const { EmbedBuilder, ChannelType, PermissionsBitField } = require("discord.js");
const { gc, setGuild } = require("../state/guildSettings");
const { COLORS } = require("../lib/embeds");

function buildSetupEmbed(guild, changes) {
  const g = gc(guild);
  return new EmbedBuilder()
    .setColor(changes.length ? COLORS.success : COLORS.info)
    .setTitle(`🛡️ Guardian setup - ${guild.name}`)
    .setDescription(changes.length
      ? `**Updated:**\n${changes.map(c => `• ${c}`).join("\n")}`
      : "Run `/setup quick` for one-command setup, or `/setup roles` / `/setup channels` / `/setup whitelist` / `/setup failsafe` to configure individual fields. Current settings:")
    .addFields(
      { name: "Mod Role",       value: g.modRoleId ? `<@&${g.modRoleId}>` : "❌ Not set", inline: true },
      { name: "Mute Role",      value: g.muteRoleId ? `<@&${g.muteRoleId}>` : "❌ Not set", inline: true },
      { name: "​",         value: "​", inline: true },
      { name: "Log Channel",    value: g.logChannelId ? `<#${g.logChannelId}>` : "❌ Not set", inline: true },
      { name: "Alert Channel",  value: g.alertChannelId ? `<#${g.alertChannelId}>` : "(uses log)", inline: true },
      { name: "Msg Log",        value: g.msgLogChannelId ? `<#${g.msgLogChannelId}>` : "❌ Not set", inline: true },
      { name: "Whitelist Users",value: g.nukeWhitelistUserIds.length ? g.nukeWhitelistUserIds.map(id => `<@${id}>`).join(", ") : "None", inline: false },
      { name: "Whitelist Roles",value: g.nukeWhitelistRoleIds.length ? g.nukeWhitelistRoleIds.map(id => `<@&${id}>`).join(", ") : "None", inline: false },
      { name: "Failsafe Roles", value: g.failsafeRoleIds.length ? g.failsafeRoleIds.map(id => `<@&${id}>`).join(", ") : "None - configure with `/setup failsafe`", inline: false },
    )
    .setFooter({ text: "Behavioral thresholds are global (.env); these identity settings are per-server." })
    .setTimestamp();
}

// /setup quick - auto-provision a working Muted role + Guardian log category/channels
// for THIS guild only. Reuses existing role/channels matched by name instead of
// duplicating them if run more than once.
async function quickSetupGuild(guild, modRoleOpt) {
  const created = []; const reused = [];

  // 1) Muted role: reuse by name if present, else create with no base permissions.
  let muteRole = guild.roles.cache.find(r => !r.managed && r.name.toLowerCase() === "muted");
  if (muteRole) reused.push(`role <@&${muteRole.id}>`);
  else {
    muteRole = await guild.roles.create({ name: "Muted", color: 0x808080, reason: "Guardian quick setup" }).catch(() => null);
    if (muteRole) created.push(`role <@&${muteRole.id}>`);
  }

  // Deny send/speak on every existing channel so the role actually mutes.
  if (muteRole) {
    for (const ch of guild.channels.cache.values()) {
      if (ch.isThread?.()) continue;
      const opts = {};
      if (ch.isTextBased?.()) Object.assign(opts, {
        SendMessages: false, AddReactions: false,
        CreatePublicThreads: false, CreatePrivateThreads: false, SendMessagesInThreads: false,
      });
      if (ch.isVoiceBased?.()) Object.assign(opts, { Speak: false, Stream: false });
      if (Object.keys(opts).length)
        await ch.permissionOverwrites.edit(muteRole, opts, { reason: "Guardian quick setup: mute role overwrite" }).catch(() => {});
    }
  }

  // 2) "Guardian" category + 3 private log channels: reuse by name if present, else create.
  let category = guild.channels.cache.find(c => c.type === ChannelType.GuildCategory && c.name === "Guardian");
  if (category) reused.push(`category **${category.name}**`);
  else {
    category = await guild.channels.create({ name: "Guardian", type: ChannelType.GuildCategory, reason: "Guardian quick setup" }).catch(() => null);
    if (category) created.push(`category **${category.name}**`);
  }

  const overwrites = [{ id: guild.id, type: 0, deny: [PermissionsBitField.Flags.ViewChannel] }];
  if (modRoleOpt) overwrites.push({ id: modRoleOpt.id, type: 0, allow: [PermissionsBitField.Flags.ViewChannel] });

  async function ensureChannel(name) {
    let ch = guild.channels.cache.find(c =>
      c.type === ChannelType.GuildText && c.name === name && (!category || c.parentId === category.id));
    if (ch) { reused.push(`<#${ch.id}>`); return ch; }
    ch = await guild.channels.create({
      name, type: ChannelType.GuildText, parent: category?.id,
      permissionOverwrites: overwrites, reason: "Guardian quick setup",
    }).catch(() => null);
    if (ch) created.push(`<#${ch.id}>`);
    return ch;
  }

  const logCh    = await ensureChannel("mod-logs");
  const alertCh  = await ensureChannel("mod-alerts");
  const msgLogCh = await ensureChannel("message-logs");

  if (muteRole)   setGuild(guild.id, "muteRoleId", muteRole.id);
  if (logCh)      setGuild(guild.id, "logChannelId", logCh.id);
  if (alertCh)    setGuild(guild.id, "alertChannelId", alertCh.id);
  if (msgLogCh)   setGuild(guild.id, "msgLogChannelId", msgLogCh.id);
  if (modRoleOpt) setGuild(guild.id, "modRoleId", modRoleOpt.id);

  return { created, reused };
}

module.exports = { buildSetupEmbed, quickSetupGuild };
