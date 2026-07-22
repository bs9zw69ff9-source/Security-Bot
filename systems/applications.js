// ── Application System (Appy-style DM interview → staff review → role grant) ──
const {
  Events, EmbedBuilder, ButtonBuilder, ButtonStyle, ActionRowBuilder,
  ModalBuilder, TextInputBuilder, TextInputStyle,
} = require("discord.js");
const client = require("../lib/client");
const { getApplications, getApplication, setApplication } = require("../state/applications");
const { isMod } = require("../lib/permissions");
const { COLORS, APPY_GREEN, APPY_BLURPLE, APPY_RED, APP_PENDING, secLog } = require("../lib/embeds");

const APP_QUESTION_TIMEOUT_MS = 10 * 60 * 1000; // per-question DM reply window
// Users with an in-progress DM interview, so we never start two at once.
const activeDmApps = new Set(); // userId
// Per-answer character cap: spread a safe budget across the questions so the
// finished review embed stays under Discord's 6000-char total, capped at the
// 1024 per-field limit.
function appAnswerCap(questionCount) {
  return Math.max(200, Math.min(1024, Math.floor(5200 / Math.max(questionCount, 1))));
}

// Requirements block shown as an application panel's description. Age and
// member-time minimums are per-app (app.minAge / app.minMemberTime), so each
// application can state its own; both fall back to sensible defaults.
function buildRequirements(app) {
  const age = app?.minAge ?? 14;
  const memberTime = app?.minMemberTime || "1 week";
  return "**REQUIREMENTS**\n" +
    `Age: ${age}\n` +
    "No Joke Applications (May result in blacklist)\n" +
    "Use of AI is not tolerated\n" +
    `Must be a member longer than ${memberTime}`;
}

function buildAppPanelEmbed(guild, app) {
  const closed = !!app.closed;
  const e = new EmbedBuilder()
    .setColor(closed ? COLORS.neutral : COLORS.info)
    .setTitle(`${app.emoji || "📝"} ${app.label} Application${closed ? " (Closed)" : ""}`)
    .setThumbnail(guild.iconURL?.() || null)
    .setFooter({ text: guild.name })
    .setTimestamp();
  if (closed) {
    e.setDescription(`**${app.label} applications are closed right now.** Check back soon.\n\n${buildRequirements(app)}`);
  } else {
    e.setDescription(buildRequirements(app));
  }
  return e;
}
// A single Apply button reflecting one app's open/closed state.
function buildApplyButton(app) {
  const closed = !!app.closed;
  return new ButtonBuilder()
    .setCustomId(`app_apply_${app.key}`)
    .setLabel(closed ? `${app.label} closed`.slice(0, 80) : `Apply for ${app.label}`.slice(0, 80))
    .setEmoji(closed ? "🔒" : (app.emoji || "📝"))
    .setStyle(closed ? ButtonStyle.Secondary : ButtonStyle.Primary)
    .setDisabled(closed);
}

// Combined panel embed for a channel that hosts 2+ applications (e.g. the
// family channel with Gambino + Colombo) - one embed, a button per app.
// If every app shares the same requirements, show one block; otherwise show
// each app's requirements under its own heading.
function buildCombinedPanelEmbed(guild, apps) {
  const blocks = apps.map(a => [a, buildRequirements(a)]);
  const unique = [...new Set(blocks.map(([, r]) => r))];
  const description = unique.length === 1
    ? unique[0]
    : blocks.map(([a, r]) => `${a.emoji || "📝"} __${a.label}__\n${r}`).join("\n\n");
  return new EmbedBuilder()
    .setColor(COLORS.info)
    .setTitle("📋 Applications")
    .setDescription(description)
    .setThumbnail(guild.iconURL?.() || null)
    .setFooter({ text: guild.name })
    .setTimestamp();
}

// Group a guild's panel-eligible apps by their panel channel.
function appsByPanelChannel(guildId) {
  const groups = new Map(); // channelId -> [apps]
  for (const app of Object.values(getApplications(guildId))) {
    if (!app.panelChannelId || !app.questions?.length) continue;
    if (!groups.has(app.panelChannelId)) groups.set(app.panelChannelId, []);
    groups.get(app.panelChannelId).push(app);
  }
  return groups;
}

// Message payload for a channel's panel: single-app style for one app, a
// combined embed with one button per app for a shared channel.
function panelPayloadForGroup(guild, apps) {
  if (apps.length === 1) return { embeds: [buildAppPanelEmbed(guild, apps[0])], components: [new ActionRowBuilder().addComponents(buildApplyButton(apps[0]))] };
  const buttons = apps.slice(0, 25).map(buildApplyButton);
  const rows = [];
  for (let i = 0; i < buttons.length; i += 5) rows.push(new ActionRowBuilder().addComponents(buttons.slice(i, i + 5)));
  return { embeds: [buildCombinedPanelEmbed(guild, apps)], components: rows };
}

// Point every app in a channel group at the same panel message id.
function setGroupPanelMessage(guildId, apps, messageId) {
  for (const a of apps) if (a.panelMessageId !== messageId) setApplication(guildId, a.key, { panelMessageId: messageId });
}

// Render (edit-in-place or post) the one panel message for a channel group, so
// open/close changes on any member app update the shared panel live.
async function renderChannelPanel(guild, channelId, apps) {
  const channel = guild.channels.cache.get(channelId);
  if (!channel) return;
  const payload = panelPayloadForGroup(guild, apps);
  const existingId = apps.map(a => a.panelMessageId).find(Boolean);
  if (existingId) {
    const existing = await channel.messages.fetch(existingId).catch(() => null);
    if (existing) { await existing.edit(payload).catch(() => {}); setGroupPanelMessage(guild.id, apps, existing.id); return; }
  }
  const posted = await channel.send(payload).catch(() => null);
  if (posted) setGroupPanelMessage(guild.id, apps, posted.id);
}

// Refresh the whole panel of the channel `app` lives in (so a combined panel's
// other buttons are rebuilt too when this one's open/closed state changes).
async function refreshAppPanel(guild, app) {
  if (!app.panelChannelId) return;
  const apps = appsByPanelChannel(guild.id).get(app.panelChannelId) || [app];
  await renderChannelPanel(guild, app.panelChannelId, apps);
}

// Post each channel's panel if it isn't already up. For a shared channel this
// also reconciles any leftover separate/duplicate panels (e.g. from before
// Gambino + Colombo were combined) down to a single combined message.
async function ensureApplicationPanels(guild) {
  for (const [channelId, apps] of appsByPanelChannel(guild.id)) {
    const channel = guild.channels.cache.get(channelId);
    if (!channel) continue;
    const ids = [...new Set(apps.map(a => a.panelMessageId).filter(Boolean))];
    const live = [];
    for (const id of ids) { const m = await channel.messages.fetch(id).catch(() => null); if (m) live.push(m); }

    // Already a single shared panel message - just refresh it to the current state.
    if (live.length === 1 && ids.length === 1) {
      await live[0].edit(panelPayloadForGroup(guild, apps)).catch(() => {});
      setGroupPanelMessage(guild.id, apps, live[0].id);
      continue;
    }
    // Otherwise (nothing up yet, or multiple stale/separate panels): clear any
    // leftovers and post one fresh panel for the channel.
    for (const m of live) await m.delete().catch(() => {});
    const posted = await channel.send(panelPayloadForGroup(guild, apps)).catch(() => null);
    if (posted) {
      setGroupPanelMessage(guild.id, apps, posted.id);
      console.log(`📝 Posted application panel (${apps.map(a => a.label).join(", ")}) in #${channel.name} (${guild.name})`);
    }
  }
}

async function handleAppApply(interaction) {
  const key = interaction.customId.replace("app_apply_", "");
  const app = getApplication(interaction.guild.id, key);
  if (!app) return interaction.reply({ content: "Sorry, that application isn't around anymore.", ephemeral: true });
  // Re-check even though the button is disabled when closed - the panel message
  // could be stale, so never let a closed application start an interview.
  if (app.closed) {
    await refreshAppPanel(interaction.guild, app).catch(() => {}); // resync the stale panel
    return interaction.reply({ content: `**${app.label}** applications are closed right now. Do check back soon!`, ephemeral: true });
  }
  if (!app.reviewChannelId) return interaction.reply({ content: "This application isn't quite ready yet. Please give an admin a heads up.", ephemeral: true });
  if (!app.questions?.length) return interaction.reply({ content: "This application doesn't have any questions set up yet. Please let an admin know.", ephemeral: true });
  if (activeDmApps.has(interaction.user.id))
    return interaction.reply({ content: "You've already got an application open in your DMs. Finish that one first, or hit **Cancel Application** there, then come back.", ephemeral: true });

  // Open a DM and send the intro BEFORE acknowledging, so a closed-DM user gets a
  // clear message instead of silently starting an interview they can't see.
  let dm, introMsg;
  try {
    dm = await interaction.user.createDM();
    introMsg = await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_GREEN)
      .setTitle("Application Started")
      .setDescription("Just answer the questions below by sending a message to the bot. Take your time, and be honest.")] });
  } catch {
    return interaction.reply({ content: "I couldn't slide into your DMs. Turn on direct messages for this server (Privacy Settings → Allow direct messages from server members), then give Apply another tap.", ephemeral: true });
  }

  // Appy-style ephemeral confirmation: a green "Application started" card with a
  // Jump-to-application link button pointing at the DM.
  const jumpRow = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setStyle(ButtonStyle.Link).setLabel("Jump to application")
      .setURL(`https://discord.com/channels/@me/${dm.id}/${introMsg.id}`));
  await interaction.reply({ ephemeral: true, components: [jumpRow], embeds: [new EmbedBuilder().setColor(APPY_GREEN)
    .setTitle("Application started")
    .setDescription("Your application's up and waiting in your DMs. Hit the button below to jump straight to it.")] });
  runDmApplication(interaction.guild, interaction.user, app, dm).catch(err => console.error("⚠️ DM application flow failed:", err));
}

// Walk the applicant through the questions in DMs, one at a time (Appy-style).
async function runDmApplication(guild, user, app, dm) {
  activeDmApps.add(user.id);
  const startedAt = Date.now();
  try {
    const cap = appAnswerCap(app.questions.length);
    const total = app.questions.length;
    const cancelRow = new ActionRowBuilder().addComponents(
      new ButtonBuilder().setCustomId("app_cancel").setLabel("Cancel Application").setStyle(ButtonStyle.Danger));
    const answers = [];

    for (let i = 0; i < total; i++) {
      const qMsg = await dm.send({
        embeds: [new EmbedBuilder().setColor(APPY_BLURPLE)
          .setTitle(`${app.label} Application`)
          .setDescription(`${i + 1}/${total}. ${app.questions[i]}\n\n-# To answer this one, just send your response as a message here.`)],
        components: [cancelRow],
      }).catch(() => null);

      // Whichever comes first: the applicant's reply, or a click on Cancel Application.
      const replyP = dm.awaitMessages({ filter: m => m.author.id === user.id, max: 1, time: APP_QUESTION_TIMEOUT_MS })
        .then(c => (c.size ? c.first() : "TIMEOUT")).catch(() => "TIMEOUT");
      const cancelP = qMsg
        ? qMsg.awaitMessageComponent({ filter: b => b.user.id === user.id && b.customId === "app_cancel", time: APP_QUESTION_TIMEOUT_MS })
            .then(b => { b.deferUpdate().catch(() => {}); return "CANCEL"; }).catch(() => "TIMEOUT")
        : Promise.resolve("TIMEOUT");
      const result = await Promise.race([replyP, cancelP]);
      if (qMsg) await qMsg.edit({ components: [] }).catch(() => {}); // retire the Cancel button for this question

      if (result === "CANCEL") {
        await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Application cancelled")
          .setDescription(`All good, I've scrapped your ${app.label} application. Nothing got sent. Swing by the panel whenever you want to give it another go.`)] }).catch(() => {});
        return;
      }
      if (result === "TIMEOUT") {
        await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Application cancelled")
          .setDescription(`Looks like you wandered off, so I've closed out your ${app.label} application for now. Start fresh from the panel whenever you're ready.`)] }).catch(() => {});
        return;
      }

      const msg = result;
      let content = (msg.content || "").trim();
      if (content.toLowerCase() === "cancel") {
        await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Application cancelled")
          .setDescription(`All good, I've scrapped your ${app.label} application. Nothing got sent.`)] }).catch(() => {});
        return;
      }
      if (!content && msg.attachments?.size) content = [...msg.attachments.values()].map(a => a.url).join("\n"); // image/file-only answer
      answers.push(content ? content.slice(0, cap) : "*(left blank)*");
    }

    // The application could have been closed or deleted mid-interview - re-check before submitting.
    const fresh = getApplication(guild.id, app.key);
    if (!fresh || fresh.closed || !fresh.reviewChannelId) {
      await dm.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Applications closed")
        .setDescription(`Ah, ${app.label} applications shut just as you were wrapping up, so this one didn't make it through. Sorry about the timing - catch it next time they open.`)] }).catch(() => {});
      return;
    }

    const ok = await finalizeApplication(guild, user, fresh, answers, startedAt);
    await dm.send({ embeds: [ok
      ? new EmbedBuilder().setColor(APPY_GREEN).setTitle("Application submitted")
          .setDescription("Your application has been submitted.\n\nThe team will give it a read and get back to you right here. Thanks for taking the time, and good luck!")
      : new EmbedBuilder().setColor(APPY_RED).setTitle("Something went wrong")
          .setDescription("Something broke on my end and your application didn't go through. Give a staff member a nudge and they'll get it sorted.")] }).catch(() => {});
  } finally {
    activeDmApps.delete(user.id);
  }
}

// Post a completed application to its review channel. Returns true on success.
async function finalizeApplication(guild, user, app, answers, startedAt) {
  const reviewChannel = guild.channels.cache.get(app.reviewChannelId);
  if (!reviewChannel) return false;

  const member = await guild.members.fetch(user.id).catch(() => null);
  const durationSec = Math.max(0, Math.round((Date.now() - (startedAt ?? Date.now())) / 1000));
  const joinedUnix = member?.joinedTimestamp ? Math.floor(member.joinedTimestamp / 1000) : null;
  const submittedUnix = Math.floor(Date.now() / 1000);
  const statsLines = [
    `UserId: \`${user.id}\``,
    `Username: \`${user.username}\``,
    `User: <@${user.id}>`,
    `Duration: \`${durationSec}s\``,
    joinedUnix ? `Joined guild <t:${joinedUnix}:R>` : null,
    `Submitted <t:${submittedUnix}:R>`,
  ].filter(Boolean).join("\n");

  const reviewEmbed = new EmbedBuilder()
    .setColor(APP_PENDING)
    .setTitle(`${user.username}'s '${app.label} Application' Application Submitted`.slice(0, 256))
    .setThumbnail(user.displayAvatarURL?.() ?? null)
    .addFields([
      ...app.questions.map((q, i) => ({
        name: `${i + 1}. ${q}`.slice(0, 256),
        value: (answers[i] || "*(left blank)*").slice(0, 1024),
        inline: false,
      })),
      { name: "Submission stats", value: statsLines.slice(0, 1024), inline: false },
    ])
    .setTimestamp();

  const row1 = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(`app_accept_${app.key}_${user.id}`).setLabel("Accept").setStyle(ButtonStyle.Success),
    new ButtonBuilder().setCustomId(`app_deny_${app.key}_${user.id}`).setLabel("Deny").setStyle(ButtonStyle.Danger),
    new ButtonBuilder().setCustomId(`app_acceptwithreason_${app.key}_${user.id}`).setLabel("Accept with reason").setStyle(ButtonStyle.Success),
  );
  const row2 = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(`app_denywithreason_${app.key}_${user.id}`).setLabel("Deny with reason").setStyle(ButtonStyle.Danger),
  );

  const posted = await reviewChannel.send({ embeds: [reviewEmbed], components: [row1, row2] }).catch(() => null);
  if (!posted) return false;
  secLog(guild, "New Application", `<@${user.id}> just applied for **${app.label}**. It's waiting for a look in <#${reviewChannel.id}>.`, COLORS.info);
  return true;
}

// Parse "app_accept_<key>_<userId>" / "app_deny_<key>_<userId>" → { key, userId }.
function parseReviewCustomId(customId, prefix) {
  const rest = customId.slice(prefix.length);
  const lastUnderscore = rest.lastIndexOf("_");
  return { key: rest.slice(0, lastUnderscore), userId: rest.slice(lastUnderscore + 1) };
}

// Apps where existing members - anyone already holding one of the app's own
// accepted (whitelist) roles - can review pending applications for that same
// app, on top of staff holding the mod role. Police + crime families only;
// staff applications still require the mod role.
const PEER_REVIEW_APP_KEYS = new Set(["nypd", "gambino", "colombo"]);
function canReviewApp(member, app) {
  if (isMod(member)) return true;
  if (!app || !PEER_REVIEW_APP_KEYS.has(app.key)) return false;
  return (app.acceptedRoleIds || []).some(id => member.roles.cache.has(id));
}

// Shared accept path for both the plain "Accept" button and the "Accept with
// reason" modal submit - grants roles, repaints the review message green with
// every button retired, then DMs the applicant.
async function performAppAccept(interaction, key, userId, reason, messageId) {
  const { guild, member } = interaction;
  const app = getApplication(guild.id, key);
  if (!app) return interaction.reply({ content: "That application type doesn't exist anymore.", ephemeral: true });

  await interaction.deferUpdate().catch(() => {});
  const applicant = await guild.members.fetch(userId).catch(() => null);

  let grantedCount = 0; const failedRoles = [];
  if (applicant) {
    for (const roleId of app.acceptedRoleIds || []) {
      const role = guild.roles.cache.get(roleId);
      if (!role) { failedRoles.push(`\`${roleId}\` (missing)`); continue; }
      if (!role.editable) { failedRoles.push(`${role.name} (above me)`); continue; }
      const ok = await applicant.roles.add(role, `Application accepted by ${member.user.tag}`).then(() => true).catch(() => false);
      if (ok) grantedCount++; else failedRoles.push(role.name);
    }
  }

  const msg = await interaction.channel.messages.fetch(messageId).catch(() => null);
  if (msg && msg.embeds[0]) {
    const updated = EmbedBuilder.from(msg.embeds[0]).setColor(APPY_GREEN);
    await msg.edit({
      embeds: [updated],
      components: [new ActionRowBuilder().addComponents(
        new ButtonBuilder().setCustomId("app_done_accept").setLabel(`Accepted by ${member.user.username}`.slice(0, 80)).setEmoji("✅").setStyle(ButtonStyle.Success).setDisabled(true))],
    }).catch(() => {});
  }

  if (applicant) await applicant.user.send({ embeds: [new EmbedBuilder().setColor(APPY_GREEN).setTitle("Application accepted")
    .setDescription(`Your application for \`${app.label} Application\` has been accepted by <@${member.id}>.${reason ? `\n\nReason: ${reason}` : ""}`)] }).catch(() => {});
  secLog(guild, "Application Accepted",
    `<@${member.id}> accepted <@${userId}>'s **${app.label}** application and handed them **${grantedCount}** role${grantedCount === 1 ? "" : "s"}.` +
    (reason ? `\nReason given: ${reason}` : "") +
    (failedRoles.length ? `\nHeads up, I couldn't grant: ${failedRoles.join(", ")}` : "") +
    (!applicant ? `\nThey've since left the server, so no roles were applied.` : ""),
    COLORS.success);
}

// Shared deny path for both the plain "Deny" button and the "Deny with
// reason" modal submit - repaints the review message red with every button
// retired, then DMs the applicant.
async function performAppDeny(interaction, key, userId, reason, messageId) {
  const { guild, member } = interaction;
  const app = getApplication(guild.id, key);

  await interaction.deferUpdate().catch(() => {});
  const msg = await interaction.channel.messages.fetch(messageId).catch(() => null);
  if (msg && msg.embeds[0]) {
    const updated = EmbedBuilder.from(msg.embeds[0]).setColor(APPY_RED);
    if (reason) updated.addFields({ name: "Reason", value: reason.slice(0, 1024), inline: false });
    await msg.edit({
      embeds: [updated],
      components: [new ActionRowBuilder().addComponents(
        new ButtonBuilder().setCustomId("app_done_deny").setLabel(`Denied by ${member.user.username}`.slice(0, 80)).setEmoji("⛔").setStyle(ButtonStyle.Danger).setDisabled(true))],
    }).catch(() => {});
  }

  const applicant = await guild.members.fetch(userId).catch(() => null);
  if (applicant) await applicant.user.send({ embeds: [new EmbedBuilder().setColor(APPY_RED).setTitle("Application denied")
    .setDescription(`Your application for \`${app?.label || "that role"} Application\` has been denied by <@${member.id}>.${reason ? `\n\nReason: ${reason}` : ""}`)] }).catch(() => {});
  secLog(guild, "Application Denied", `<@${member.id}> turned down <@${userId}>'s **${app?.label || key}** application.${reason ? ` Reason given: ${reason}` : ""}`, COLORS.danger);
}

// "Accept" - immediate, no reason prompt.
async function handleAppAccept(interaction) {
  const { guild, member } = interaction;
  const { key, userId } = parseReviewCustomId(interaction.customId, "app_accept_");
  const app = getApplication(guild.id, key);
  if (!app) return interaction.reply({ content: "That application type doesn't exist anymore.", ephemeral: true });
  if (!canReviewApp(member, app)) return interaction.reply({ content: "Only staff, or a whitelisted member of this app, can review applications.", ephemeral: true });
  return performAppAccept(interaction, key, userId, null, interaction.message.id);
}

// "Accept with reason" - opens a modal, actual grant happens on submit.
async function handleAppAcceptWithReason(interaction) {
  const { guild, member } = interaction;
  const { key, userId } = parseReviewCustomId(interaction.customId, "app_acceptwithreason_");
  const app = getApplication(guild.id, key);
  if (!app) return interaction.reply({ content: "That application type doesn't exist anymore.", ephemeral: true });
  if (!canReviewApp(member, app)) return interaction.reply({ content: "Only staff, or a whitelisted member of this app, can review applications.", ephemeral: true });

  const modal = new ModalBuilder().setCustomId(`app_acceptreason_${key}_${userId}_${interaction.message.id}`).setTitle(`Accept ${app.label} Application`.slice(0, 45));
  modal.addComponents(new ActionRowBuilder().addComponents(
    new TextInputBuilder().setCustomId("reason").setLabel("Reason (optional, shared with them)").setStyle(TextInputStyle.Paragraph).setRequired(false).setMaxLength(500)));
  return interaction.showModal(modal);
}

async function handleAppAcceptReasonSubmit(interaction) {
  // customId: app_acceptreason_<key>_<userId>_<messageId>
  const rest = interaction.customId.slice("app_acceptreason_".length);
  const parts = rest.split("_");
  const messageId = parts.pop();
  const userId = parts.pop();
  const key = parts.join("_");
  const reason = interaction.fields.getTextInputValue("reason")?.trim();
  return performAppAccept(interaction, key, userId, reason || null, messageId);
}

// "Deny" - immediate, no reason prompt.
async function handleAppDeny(interaction) {
  const { guild, member } = interaction;
  const { key, userId } = parseReviewCustomId(interaction.customId, "app_deny_");
  const app = getApplication(guild.id, key);
  if (!app) return interaction.reply({ content: "That application type doesn't exist anymore.", ephemeral: true });
  if (!canReviewApp(member, app)) return interaction.reply({ content: "Only staff, or a whitelisted member of this app, can review applications.", ephemeral: true });
  return performAppDeny(interaction, key, userId, null, interaction.message.id);
}

// "Deny with reason" - opens a modal, actual denial happens on submit.
async function handleAppDenyWithReason(interaction) {
  const { guild, member } = interaction;
  const { key, userId } = parseReviewCustomId(interaction.customId, "app_denywithreason_");
  const app = getApplication(guild.id, key);
  if (!app) return interaction.reply({ content: "That application type doesn't exist anymore.", ephemeral: true });
  if (!canReviewApp(member, app)) return interaction.reply({ content: "Only staff, or a whitelisted member of this app, can review applications.", ephemeral: true });

  const modal = new ModalBuilder().setCustomId(`app_denyreason_${key}_${userId}_${interaction.message.id}`).setTitle(`Deny ${app.label} Application`.slice(0, 45));
  modal.addComponents(new ActionRowBuilder().addComponents(
    new TextInputBuilder().setCustomId("reason").setLabel("Reason (optional, shared with them)").setStyle(TextInputStyle.Paragraph).setRequired(false).setMaxLength(500)));
  return interaction.showModal(modal);
}

async function handleAppDenyReason(interaction) {
  // customId: app_denyreason_<key>_<userId>_<messageId>
  const rest = interaction.customId.slice("app_denyreason_".length);
  const parts = rest.split("_");
  const messageId = parts.pop();
  const userId = parts.pop();
  const key = parts.join("_");
  const reason = interaction.fields.getTextInputValue("reason")?.trim();
  return performAppDeny(interaction, key, userId, reason || null, messageId);
}

// ── Application buttons (apply / accept / deny) ────────────────
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isButton() || !interaction.inGuild()) return;
  try {
    if (interaction.customId.startsWith("app_apply_"))  return await handleAppApply(interaction);
    if (interaction.customId.startsWith("app_acceptwithreason_")) return await handleAppAcceptWithReason(interaction);
    if (interaction.customId.startsWith("app_accept_")) return await handleAppAccept(interaction);
    if (interaction.customId.startsWith("app_denywithreason_")) return await handleAppDenyWithReason(interaction);
    if (interaction.customId.startsWith("app_deny_"))   return await handleAppDeny(interaction);
  } catch (err) {
    console.error("⚠️ application button handler failed:", err);
    const msg = { content: "⚠️ Something went wrong.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});

// ── Application accept/deny-reason modal submits (staff review only) ───
client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isModalSubmit() || !interaction.inGuild()) return;
  const isAccept = interaction.customId.startsWith("app_acceptreason_");
  const isDeny = interaction.customId.startsWith("app_denyreason_");
  if (!isAccept && !isDeny) return;
  try {
    if (isAccept) await handleAppAcceptReasonSubmit(interaction);
    else await handleAppDenyReason(interaction);
  } catch (err) {
    console.error("⚠️ application modal handler failed:", err);
    const msg = { content: "⚠️ Something went wrong.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});

module.exports = {
  appsByPanelChannel, renderChannelPanel, refreshAppPanel, ensureApplicationPanels,
  handleAppApply,
};
