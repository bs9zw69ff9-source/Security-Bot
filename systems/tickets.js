// ── Ticket System ────────────────────────────────────────────
const {
  Events, EmbedBuilder, ChannelType, PermissionsBitField,
  ButtonBuilder, ButtonStyle, ActionRowBuilder,
  ModalBuilder, TextInputBuilder, TextInputStyle,
} = require("discord.js");
const client = require("../lib/client");
const { gc } = require("../state/guildSettings");
const {
  getTicketConfig, setTicketConfig,
  getOpenTicket, setOpenTicket, deleteOpenTicket, findOpenTicketByUser,
} = require("../state/tickets");
const { isMod } = require("../lib/permissions");
const { COLORS, embed, secLog, formatUptime } = require("../lib/embeds");

function buildTicketPanelEmbed(guild, cfg) {
  return new EmbedBuilder()
    .setColor(COLORS.info)
    .setTitle("🎫 Support Tickets")
    .setDescription(
      "Need a hand? Pick the option below that fits what you need, and I'll open a private " +
      "ticket just for you and the team.\n\n" +
      cfg.types.map(t => `${t.emoji || "🎫"}  **${t.label}**`).join("\n") +
      "\n\nSomeone will be with you as soon as they can. Please stick to one ticket at a time."
    )
    .setThumbnail(guild.iconURL?.() || null)
    .setFooter({ text: guild.name })
    .setTimestamp();
}
function buildTicketPanelRows(cfg) {
  const buttons = cfg.types.slice(0, 25).map(t =>
    new ButtonBuilder().setCustomId(`ticket_open_${t.key}`).setLabel(t.label).setEmoji(t.emoji || "🎫").setStyle(ButtonStyle.Secondary));
  const rows = [];
  for (let i = 0; i < buttons.length; i += 5) rows.push(new ActionRowBuilder().addComponents(buttons.slice(i, i + 5)));
  return rows;
}

// Post the panel (or leave it alone if it's already posted and the message
// still exists) - called on boot for every guild with ticket types configured.
async function ensureTicketPanel(guild) {
  const cfg = getTicketConfig(guild.id);
  if (!cfg.types.length || !cfg.panelChannelId) return;
  const channel = guild.channels.cache.get(cfg.panelChannelId);
  if (!channel) return;
  if (cfg.panelMessageId) {
    const existing = await channel.messages.fetch(cfg.panelMessageId).catch(() => null);
    if (existing) return;
  }
  const posted = await channel.send({ embeds: [buildTicketPanelEmbed(guild, cfg)], components: buildTicketPanelRows(cfg) }).catch(() => null);
  if (posted) {
    setTicketConfig(guild.id, { panelMessageId: posted.id });
    console.log(`🎫 Posted ticket panel in #${channel.name} (${guild.name})`);
  }
}

function escapeHtml(str) {
  return String(str ?? "").replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

// Paginate through the whole channel history, oldest first.
async function fetchAllMessages(channel) {
  let all = []; let lastId;
  for (;;) {
    const batch = await channel.messages.fetch({ limit: 100, ...(lastId ? { before: lastId } : {}) }).catch(() => null);
    if (!batch || !batch.size) break;
    all = all.concat([...batch.values()]);
    lastId = batch.last().id;
    if (batch.size < 100) break;
  }
  return all.reverse();
}

// Self-contained, dependency-free HTML transcript (dark-themed to resemble Discord).
async function buildTranscript(channel, ticket, type, closerTag) {
  const messages = await fetchAllMessages(channel);
  const rows = messages.map(m => {
    const time = new Date(m.createdTimestamp).toISOString().replace("T", " ").slice(0, 19) + " UTC";
    const author = escapeHtml(m.author?.tag || "Unknown");
    const avatar = escapeHtml(m.author?.displayAvatarURL?.({ size: 64 }) || "");
    const content = escapeHtml(m.content || "").replace(/\n/g, "<br>");
    const atts = [...m.attachments.values()]
      .map(a => `<div class="att"><a href="${escapeHtml(a.url)}" target="_blank" rel="noopener">📎 ${escapeHtml(a.name || "attachment")}</a></div>`)
      .join("");
    return `<div class="msg">${avatar ? `<img class="avatar" src="${avatar}">` : `<div class="avatar"></div>`}<div class="body"><div class="meta"><span class="author">${author}</span><span class="time">${time}</span></div><div class="content">${content || "<i>(no text content)</i>"}</div>${atts}</div></div>`;
  }).join("\n");

  return `<!doctype html>
<html><head><meta charset="utf-8"><title>Transcript - #${escapeHtml(channel.name)}</title>
<style>
  body { background:#313338; color:#dbdee1; font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif; margin:0; padding:24px; }
  .header { border-bottom:1px solid #3f4147; padding-bottom:16px; margin-bottom:16px; }
  .header h1 { margin:0 0 4px; font-size:20px; color:#f2f3f5; }
  .header .sub { color:#949ba4; font-size:13px; }
  .msg { display:flex; gap:12px; padding:8px 0; }
  .avatar { width:40px; height:40px; border-radius:50%; flex-shrink:0; background:#5865f2; }
  .meta { font-size:13px; margin-bottom:2px; }
  .author { font-weight:600; color:#f2f3f5; }
  .time { color:#949ba4; margin-left:8px; }
  .content { font-size:15px; line-height:1.4; white-space:pre-wrap; word-wrap:break-word; }
  .att { margin-top:4px; }
  .att a { color:#00a8fc; text-decoration:none; }
</style></head>
<body>
  <div class="header">
    <h1>🎫 ${escapeHtml(type?.label || ticket.typeKey)} - #${escapeHtml(channel.name)}</h1>
    <div class="sub">Opened by &lt;${escapeHtml(ticket.openerId)}&gt; · Closed by ${escapeHtml(closerTag || "unknown")} · ${messages.length} message(s)</div>
  </div>
  ${rows || "<p><i>No messages were sent in this ticket.</i></p>"}
</body></html>`;
}

async function createTicketChannel(interaction, key, reason) {
  const { guild, member } = interaction;
  const cfg = getTicketConfig(guild.id);
  const type = cfg.types.find(t => t.key === key);
  if (!type) return interaction.reply({ content: "Sorry, that ticket option isn't available anymore.", ephemeral: true });

  const existing = findOpenTicketByUser(guild.id, member.id, key);
  if (existing && guild.channels.cache.has(existing))
    return interaction.reply({ content: `You've already got one open over here: <#${existing}>`, ephemeral: true });

  await interaction.deferReply({ ephemeral: true });

  let category = cfg.categoryId ? guild.channels.cache.get(cfg.categoryId) : null;
  if (!category) {
    category = guild.channels.cache.find(c => c.type === ChannelType.GuildCategory && c.name === "Tickets");
    if (!category) category = await guild.channels.create({ name: "Tickets", type: ChannelType.GuildCategory, reason: "Ticket system: auto-created category" }).catch(() => null);
    if (category) setTicketConfig(guild.id, { categoryId: category.id });
  }

  // Explicit `type` (0 = role, 1 = member) on every overwrite - without it,
  // discord.js tries to guess by checking caches and throws "Supplied
  // parameter is not a cached User or Role" whenever it can't resolve one
  // (e.g. a modRoleId that isn't cached at that instant).
  const g = gc(guild);
  const overwrites = [
    { id: guild.id, type: 0, deny: [PermissionsBitField.Flags.ViewChannel] },
    { id: member.id, type: 1, allow: [PermissionsBitField.Flags.ViewChannel, PermissionsBitField.Flags.SendMessages, PermissionsBitField.Flags.ReadMessageHistory, PermissionsBitField.Flags.AttachFiles] },
  ];
  if (g.modRoleId) overwrites.push({ id: g.modRoleId, type: 0, allow: [PermissionsBitField.Flags.ViewChannel, PermissionsBitField.Flags.SendMessages, PermissionsBitField.Flags.ReadMessageHistory, PermissionsBitField.Flags.ManageMessages] });

  const safeName = (member.user.username || "user").toLowerCase().replace(/[^a-z0-9-]/g, "-").slice(0, 20) || "user";
  const channelName = `${type.key.replace(/_/g, "-")}-${safeName}`.slice(0, 90);

  let createErr = null;
  let ticketChannel = await guild.channels.create({
    name: channelName, type: ChannelType.GuildText, parent: category?.id,
    permissionOverwrites: overwrites, reason: `Ticket opened by ${member.user.tag}`,
    topic: `${type.label} ticket for ${member.user.tag} (${member.id})`,
  }).catch(e => { createErr = e; return null; });

  // If it failed while assigned to a category, retry once without a parent -
  // covers a full/invalid/stale category without fully blocking ticket creation.
  if (!ticketChannel && category) {
    ticketChannel = await guild.channels.create({
      name: channelName, type: ChannelType.GuildText,
      permissionOverwrites: overwrites, reason: `Ticket opened by ${member.user.tag}`,
      topic: `${type.label} ticket for ${member.user.tag} (${member.id})`,
    }).catch(e => { createErr = e; return null; });
  }

  if (!ticketChannel)
    return interaction.editReply(`Hmm, I couldn't open a ticket channel: \`${createErr?.message || "unknown error"}\`. Please double-check I have the Manage Channels permission.`);

  setOpenTicket(guild.id, ticketChannel.id, { typeKey: key, openerId: member.id, openedAt: Date.now(), claimedBy: null, reason });

  const welcomeEmbed = new EmbedBuilder()
    .setColor(COLORS.info)
    .setTitle(`${type.emoji || "🎫"} ${type.label}`)
    .setDescription(`Thanks for reaching out, <@${member.id}> - someone from the team will be with you shortly. Here's what you told us:\n\n${reason}`)
    .addFields(
      { name: "Opened by", value: `<@${member.id}>`, inline: true },
      { name: "Category", value: type.label, inline: true },
      { name: "Status", value: "🟢 Open, waiting for staff", inline: true },
    )
    .setFooter({ text: `Ticket ID: ${ticketChannel.id}` })
    .setTimestamp();
  const controlRow = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId("ticket_claim").setLabel("Claim").setEmoji("🙋").setStyle(ButtonStyle.Primary),
    new ButtonBuilder().setCustomId("ticket_close").setLabel("Close Ticket").setEmoji("🔒").setStyle(ButtonStyle.Danger),
  );
  const pingRole = g.modRoleId ? `<@&${g.modRoleId}> ` : "";
  await ticketChannel.send({ content: `${pingRole}<@${member.id}>`, embeds: [welcomeEmbed], components: [controlRow] }).catch(() => {});

  secLog(guild, "Ticket Opened", `<@${member.id}> opened a **${type.label}** ticket over in <#${ticketChannel.id}>.`, COLORS.info);
  return interaction.editReply(`You're all set - your ticket's open here: <#${ticketChannel.id}>`);
}

async function handleTicketOpen(interaction) {
  const { guild, customId } = interaction;
  const key = customId.replace("ticket_open_", "");
  const cfg = getTicketConfig(guild.id);
  const type = cfg.types.find(t => t.key === key);
  if (!type) return interaction.reply({ content: "Sorry, that ticket option isn't available anymore.", ephemeral: true });

  const existing = findOpenTicketByUser(guild.id, interaction.member.id, key);
  if (existing && guild.channels.cache.has(existing))
    return interaction.reply({ content: `You've already got one open over here: <#${existing}>`, ephemeral: true });

  const modal = new ModalBuilder().setCustomId(`ticket_reason_${key}`).setTitle(`${type.label} - Ticket`.slice(0, 45));
  const reasonInput = new TextInputBuilder()
    .setCustomId("reason").setLabel("What can we help you with?").setStyle(TextInputStyle.Paragraph)
    .setRequired(true).setMaxLength(1000).setPlaceholder("A few details go a long way (who, what, when)...");
  modal.addComponents(new ActionRowBuilder().addComponents(reasonInput));
  return interaction.showModal(modal);
}

async function handleTicketClaim(interaction) {
  const { guild, member, channel } = interaction;
  const ticket = getOpenTicket(guild.id, channel.id);
  if (!ticket) return interaction.reply({ content: "This isn't an active ticket channel.", ephemeral: true });
  if (!isMod(member)) return interaction.reply({ content: "Only staff can claim tickets.", ephemeral: true });
  if (ticket.claimedBy) return interaction.reply({ content: `This one's already claimed by <@${ticket.claimedBy}>.`, ephemeral: true });

  ticket.claimedBy = member.id;
  setOpenTicket(guild.id, channel.id, ticket);

  const oldEmbed = interaction.message.embeds[0];
  if (oldEmbed) {
    const newEmbed = EmbedBuilder.from(oldEmbed).spliceFields(2, 1, { name: "Status", value: `🟡 Claimed by <@${member.id}>`, inline: true });
    await interaction.update({ embeds: [newEmbed] }).catch(() => interaction.deferUpdate().catch(() => {}));
  } else {
    await interaction.deferUpdate().catch(() => {});
  }
  await channel.send({ embeds: [embed(COLORS.warn, `<@${member.id}> has got this one and will help you out from here.`)] }).catch(() => {});
}

async function handleTicketClose(interaction) {
  const { guild, member, channel } = interaction;
  const ticket = getOpenTicket(guild.id, channel.id);
  if (!ticket) return interaction.reply({ content: "This isn't an active ticket channel.", ephemeral: true });
  if (!isMod(member) && member.id !== ticket.openerId)
    return interaction.reply({ content: "Only staff or the person who opened this can close it.", ephemeral: true });

  await interaction.reply({ embeds: [embed(COLORS.warn, "Closing this ticket and saving a transcript, one sec...")] }).catch(() => {});

  const cfg = getTicketConfig(guild.id);
  const type = cfg.types.find(t => t.key === ticket.typeKey);
  const transcript = await buildTranscript(channel, ticket, type, member.user.tag);

  const logChannel = type?.logChannelId ? guild.channels.cache.get(type.logChannelId) : null;
  const openerUser = await client.users.fetch(ticket.openerId).catch(() => null);
  const summaryEmbed = new EmbedBuilder()
    .setColor(COLORS.neutral)
    .setTitle(`🔒 Ticket Closed - ${type?.label || ticket.typeKey}`)
    .addFields(
      { name: "Opened by", value: openerUser ? `${openerUser.tag} (\`${ticket.openerId}\`)` : `\`${ticket.openerId}\``, inline: true },
      { name: "Closed by", value: `<@${member.id}>`, inline: true },
      { name: "Claimed by", value: ticket.claimedBy ? `<@${ticket.claimedBy}>` : "Unclaimed", inline: true },
      { name: "Opened", value: `<t:${Math.floor(ticket.openedAt / 1000)}:F>`, inline: true },
      { name: "Duration", value: formatUptime(Date.now() - ticket.openedAt), inline: true },
      { name: "Reason", value: (ticket.reason || "N/A").slice(0, 1024), inline: false },
    )
    .setTimestamp();

  if (logChannel) {
    await logChannel.send({
      embeds: [summaryEmbed],
      files: [{ attachment: Buffer.from(transcript, "utf8"), name: `transcript-${channel.name}.html` }],
    }).catch(() => {});
  }

  secLog(guild, "Ticket Closed", `<@${member.id}> closed the **${type?.label || ticket.typeKey}** ticket that <@${ticket.openerId}> opened (<#${channel.id}>).`, COLORS.neutral);
  deleteOpenTicket(guild.id, channel.id);

  await channel.send("All done here - this channel will disappear in a few seconds.").catch(() => {});
  setTimeout(() => channel.delete("Ticket closed").catch(() => {}), 5000);
}

// ── Ticket buttons (panel + in-ticket controls) ────────────────
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isButton() || !interaction.inGuild()) return;
  try {
    if (interaction.customId.startsWith("ticket_open_")) return await handleTicketOpen(interaction);
    if (interaction.customId === "ticket_claim") return await handleTicketClaim(interaction);
    if (interaction.customId === "ticket_close") return await handleTicketClose(interaction);
  } catch (err) {
    console.error("⚠️ ticket button handler failed:", err);
    const msg = { content: "⚠️ Something went wrong.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});

// ── Ticket "reason" modal submit → actually creates the channel ──
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isModalSubmit() || !interaction.inGuild()) return;
  if (!interaction.customId.startsWith("ticket_reason_")) return;
  try {
    const key = interaction.customId.replace("ticket_reason_", "");
    const reason = interaction.fields.getTextInputValue("reason");
    await createTicketChannel(interaction, key, reason);
  } catch (err) {
    console.error("⚠️ ticket modal handler failed:", err);
    const msg = { content: "⚠️ Something went wrong opening your ticket.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});

module.exports = {
  buildTicketPanelEmbed, buildTicketPanelRows, ensureTicketPanel,
  createTicketChannel, handleTicketOpen, handleTicketClaim, handleTicketClose,
};
