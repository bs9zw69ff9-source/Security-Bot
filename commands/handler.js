// ── Slash Command Handler ─────────────────────────────────────
const { Events, EmbedBuilder, PermissionsBitField } = require("discord.js");
const client = require("../lib/client");
const { config, BOT_OWNER_IDS } = require("../lib/config");
const { COLORS, embed, secLog, alertOwner, buildBar, usageFooter, limitDeniedEmbed, renderAntiPingResponse, formatUptime } = require("../lib/embeds");
const { isOwner, isMod, isWhitelisted, canActOn, tryDM } = require("../lib/permissions");
const { gc, setGuild } = require("../state/guildSettings");
const { checkModLimit, recordModAction } = require("../state/modRates");
const { isLockdown, setLockdown, clearLockdown, lockdownState } = require("../state/lockdown");
const { ap, setAntiPing, antiPingDefaults } = require("../state/antiPing");
const { mutedRoles } = require("../state/mutedRoles");
const { addWarning, getWarnings, clearWarnings } = require("../state/warnings");
const { muteUser, unmuteUser } = require("../systems/mute");
const { bump, resetBump, nukeResponse } = require("../systems/antiNuke");
const { buildSetupEmbed, quickSetupGuild } = require("../systems/setupHelpers");
const { getTicketConfig, setTicketConfig } = require("../state/tickets");
const { buildTicketPanelEmbed, buildTicketPanelRows } = require("../systems/tickets");
const { getApplications, getApplication, setApplication } = require("../state/applications");
const { appsByPanelChannel, renderChannelPanel, refreshAppPanel } = require("../systems/applications");
const { buildPoliceManualEmbed } = require("../systems/policeManual");
const { getChainKeys, getChain, setChain } = require("../state/chainOfCommand");
const { renderChainOfCommand } = require("../systems/chainOfCommand");

client.on(Events.InteractionCreate, async (interaction) => {
  if (!interaction.isChatInputCommand()) return;
  if (!interaction.inGuild())
    return interaction.reply({ content: "You can only use this in a server.", ephemeral: true });
  const { commandName, guild, member } = interaction;

  try {
  switch (commandName) {

    // ── /mute ──────────────────────────────────────────────
    case "mute": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target  = interaction.options.getMember("user");
      const minutes = interaction.options.getInteger("minutes") ?? 10;
      const reason  = interaction.options.getString("reason") ?? "No reason provided";
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });
      const muteRoleId = gc(guild).muteRoleId;
      if (!muteRoleId || !guild.roles.cache.get(muteRoleId))
        return interaction.reply({ content: "There is no mute role set up yet. Run `/setup quick`, or set one with `/setup roles mute_role:@Role`.", ephemeral: true });

      if (!isWhitelisted(member)) {
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "mute");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("mute", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "mute");
      }
      const ok = await muteUser(target, minutes, reason);
      if (!ok) return interaction.reply({ content: "There is no mute role set up yet. Run `/setup quick`, or set one with `/setup roles mute_role:@Role`.", ephemeral: true });
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "mute");
      const stashed = mutedRoles[guild.id]?.[target.id]?.roles?.length ?? 0;
      const e = new EmbedBuilder().setColor(COLORS.muted).setTitle("🔇 Member Muted")
        .setDescription(`Muted <@${target.id}> for **${minutes > 0 ? minutes + " minutes" : "as long as it takes"}**.\n**Reason:** ${reason}\nI've set aside **${stashed}** role${stashed === 1 ? "" : "s"} and will hand them back on unmute.`)
        .setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("mute", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /unmute ────────────────────────────────────────────
    case "unmute": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target = interaction.options.getMember("user");
      if (!target) return interaction.reply({ content: "I couldn't find that user.", ephemeral: true });
      if (!gc(guild).muteRoleId) return interaction.reply({ content: "There is no mute role set up yet. Run `/setup quick`, or set one with `/setup roles mute_role:@Role`.", ephemeral: true });
      const stashed = mutedRoles[guild.id]?.[target.id]?.roles?.length ?? 0;
      await unmuteUser(guild, target.id, `Manual unmute by ${interaction.user.tag}`);
      return interaction.reply({ embeds: [new EmbedBuilder().setColor(COLORS.success).setTitle("🔊 Member Unmuted")
        .setDescription(`<@${target.id}> is unmuted, and I gave back **${stashed}** stashed role${stashed === 1 ? "" : "s"}.`).setTimestamp()] });
    }

    // ── /kick ──────────────────────────────────────────────
    case "kick": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target = interaction.options.getMember("user");
      const reason = interaction.options.getString("reason") ?? "No reason provided";
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });

      if (!isWhitelisted(member)) {
        if (bump(guild.id, member.id, "kicks", config.nukeKickThreshold)) {
          resetBump(guild.id, member.id, "kicks");
          await interaction.reply({ content: "Hold on - that just tripped the anti-nuke protection.", ephemeral: true });
          return nukeResponse(guild, member, `Issued ${config.nukeKickThreshold}+ kicks via commands in ${config.nukeWindowMs / 1000}s`);
        }
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "kick");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("kick", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "kick");
      }
      await tryDM(target.user, `You've been kicked from **${guild.name}**.\nReason: ${reason}`);
      await target.kick(reason).catch(() => {});
      secLog(guild, "Member Kicked", `<@${member.id}> kicked <@${target.id}> - ${reason}`, COLORS.danger);
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "kick");
      const e = new EmbedBuilder().setColor(COLORS.danger).setTitle("👢 Member Kicked")
        .setDescription(`Kicked <@${target.id}>.\n**Reason:** ${reason}`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("kick", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /ban ───────────────────────────────────────────────
    case "ban": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target     = interaction.options.getMember("user");
      const reason     = interaction.options.getString("reason") ?? "No reason provided";
      const deleteDays = interaction.options.getInteger("delete_days") ?? 0;
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });

      if (!isWhitelisted(member)) {
        if (bump(guild.id, member.id, "bans", config.nukeBanThreshold)) {
          resetBump(guild.id, member.id, "bans");
          await interaction.reply({ content: "Hold on - that just tripped the anti-nuke protection.", ephemeral: true });
          return nukeResponse(guild, member, `Issued ${config.nukeBanThreshold}+ bans via commands in ${config.nukeWindowMs / 1000}s`);
        }
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "ban");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("ban", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "ban");
      }
      await tryDM(target.user, `You've been banned from **${guild.name}**.\nReason: ${reason}`);
      await target.ban({ reason, deleteMessageSeconds: deleteDays * 86400 }).catch(() => {});
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "ban");
      secLog(guild, "Member Banned", `<@${member.id}> banned <@${target.id}> - ${reason}`, COLORS.danger);
      const e = new EmbedBuilder().setColor(COLORS.danger).setTitle("🔨 Member Banned")
        .setDescription(`Banned <@${target.id}>.\n**Reason:** ${reason}`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("ban", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /unban ─────────────────────────────────────────────
    case "unban": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const userId = interaction.options.getString("user_id").trim();
      const reason = interaction.options.getString("reason") ?? "No reason provided";
      if (!/^\d{17,20}$/.test(userId)) return interaction.reply({ content: "That doesn't look like a valid user ID.", ephemeral: true });
      const ban = await guild.bans.fetch(userId).catch(() => null);
      if (!ban) return interaction.reply({ content: "That user isn't banned.", ephemeral: true });
      await guild.bans.remove(userId, `Unban by ${interaction.user.tag}: ${reason}`).catch(() => {});
      secLog(guild, "Member Unbanned", `<@${member.id}> lifted the ban on \`${userId}\` - ${reason}`, COLORS.success);
      return interaction.reply({ embeds: [new EmbedBuilder().setColor(COLORS.success).setTitle("♻️ Member Unbanned")
        .setDescription(`<@${userId}> (\`${userId}\`) is unbanned.\n**Reason:** ${reason}`).setTimestamp()] });
    }

    // ── /purge ─────────────────────────────────────────────
    case "purge": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const count      = interaction.options.getInteger("count");
      const filterUser = interaction.options.getUser("user");

      if (!isWhitelisted(member)) {
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "purge");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("purge", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "purge");
      }
      await interaction.deferReply({ ephemeral: true });
      let messages = await interaction.channel.messages.fetch({ limit: 100 }).catch(() => null);
      if (!messages) return interaction.editReply("I couldn't fetch the messages here to clear them.");
      if (filterUser) messages = messages.filter(m => m.author.id === filterUser.id);
      const toDelete = [...messages.values()].slice(0, count);
      const deleted  = await interaction.channel.bulkDelete(toDelete, true).catch(() => null);
      const n = deleted?.size ?? 0;
      secLog(guild, "Purge", `<@${member.id}> cleared **${n}** message${n === 1 ? "" : "s"} in <#${interaction.channelId}>${filterUser ? ` from <@${filterUser.id}>` : ""}.`, COLORS.warn);
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "purge");
      const e = new EmbedBuilder().setColor(COLORS.warn).setTitle("🗑️ Messages Cleared")
        .setDescription(`Cleared **${n}** message${n === 1 ? "" : "s"}${filterUser ? ` from <@${filterUser.id}>` : ""}.`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("purge", newUsed, limit) });
      return interaction.editReply({ embeds: [e] });
    }

    // ── /lockdown ──────────────────────────────────────────
    case "lockdown": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const action  = interaction.options.getString("action");
      const channel = interaction.options.getChannel("channel") ?? interaction.channel;
      const lock    = action === "lock";

      if (lock && !isWhitelisted(member)) {
        if (bump(guild.id, member.id, "chLock", config.nukeChannelThreshold)) {
          resetBump(guild.id, member.id, "chLock");
          await interaction.reply({ content: "Hold on - that just tripped the anti-nuke protection.", ephemeral: true });
          return nukeResponse(guild, member, `Locked ${config.nukeChannelThreshold}+ channels via commands in ${config.nukeWindowMs / 1000}s`);
        }
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "lockdown");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("lockdown", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "lockdown");
      }
      await channel.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: lock ? false : null }).catch(() => {});
      secLog(guild, lock ? "Channel Locked" : "Channel Unlocked",
        `<@${member.id}> ${lock ? "locked down" : "reopened"} <#${channel.id}>.`, lock ? COLORS.danger : COLORS.success);
      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "lockdown");
      const e = new EmbedBuilder().setColor(lock ? COLORS.danger : COLORS.success)
        .setTitle(lock ? "🔒 Channel Locked" : "🔓 Channel Unlocked")
        .setDescription(`<#${channel.id}> is now ${lock ? "locked down - only staff can send messages" : "back open"}.`).setTimestamp();
      if (lock && !isWhitelisted(member)) e.setFooter({ text: usageFooter("lockdown", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /panic (owner only) - toggles: run again to lift ────
    case "panic": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "This one's owner only.", ephemeral: true });
      await interaction.deferReply({ ephemeral: true });

      if (isLockdown(guild.id)) {
        let unlocked = 0;
        for (const ch of guild.channels.cache.values()) {
          if (ch.isTextBased() && !ch.isThread()) {
            const ok = await ch.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: null }).then(() => true).catch(() => false);
            if (ok) unlocked++;
          }
        }
        clearLockdown(guild.id);
        alertOwner(guild, `<@${member.id}> lifted the panic lockdown. **${unlocked}** channels are back open.`, COLORS.success, "Panic Lockdown Lifted");
        return interaction.editReply(`Done - panic lockdown lifted and **${unlocked}** text channels are back open.`);
      }

      let locked = 0;
      for (const ch of guild.channels.cache.values()) {
        if (ch.isTextBased() && !ch.isThread()) {
          const ok = await ch.permissionOverwrites.edit(guild.roles.everyone, { SendMessages: false }).then(() => true).catch(() => false);
          if (ok) locked++;
        }
      }
      setLockdown(guild.id, "panic", null);
      alertOwner(guild, `<@${member.id}> hit the panic button and locked down **${locked}** channels. Run \`/panic\` again to lift it.`, COLORS.nuke, "Panic Lockdown");
      return interaction.editReply(`Panic lockdown is on - I've locked **${locked}** text channels. Run \`/panic\` again to lift it.`);
    }

    // ── /warn ──────────────────────────────────────────────
    case "warn": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target = interaction.options.getMember("user");
      const reason = interaction.options.getString("reason") ?? "No reason provided";
      const guard = canActOn(member, target);
      if (!guard.ok) return interaction.reply({ content: guard.why, ephemeral: true });

      if (!isWhitelisted(member)) {
        const { allowed, used, limit, resetsInMin } = checkModLimit(guild.id, member.id, "warn");
        if (!allowed) return interaction.reply({ embeds: [limitDeniedEmbed("warn", used, limit, resetsInMin)], ephemeral: true });
        recordModAction(guild.id, member.id, "warn");
      }
      const total = addWarning(guild.id, target.id, reason, member.id);
      await tryDM(target.user, `You've picked up a warning in **${guild.name}** (that's #${total}). Reason: ${reason}`);
      secLog(guild, "Warning Issued", `<@${member.id}> warned <@${target.id}> - that's **${total}** now. Reason: ${reason}`, COLORS.warn);

      // Escalation
      let escalation = "";
      if (config.warnBanAt && total >= config.warnBanAt) {
        await target.ban({ reason: `Auto-escalation: reached ${total} warnings` }).catch(() => {});
        escalation = `\n🔨 That hit **${total}** warnings, so they've been auto-banned.`;
        secLog(guild, "Auto-Escalation", `<@${target.id}> hit ${total} warnings and was auto-banned.`, COLORS.danger);
      } else if (config.warnKickAt && total >= config.warnKickAt) {
        await target.kick(`Auto-escalation: reached ${total} warnings`).catch(() => {});
        escalation = `\n👢 That hit **${total}** warnings, so they've been auto-kicked.`;
        secLog(guild, "Auto-Escalation", `<@${target.id}> hit ${total} warnings and was auto-kicked.`, COLORS.danger);
      } else if (config.warnMuteAt && total >= config.warnMuteAt) {
        await muteUser(target, config.warnMuteMin, `Auto-escalation: reached ${total} warnings`);
        escalation = `\n🔇 That hit **${total}** warnings, so they've been auto-muted for ${config.warnMuteMin} min.`;
      }

      const { used: newUsed, limit } = checkModLimit(guild.id, member.id, "warn");
      const e = new EmbedBuilder().setColor(COLORS.warn).setTitle("⚠️ Warning Issued")
        .setDescription(`Warned <@${target.id}>. **That's ${total} in total.**\n**Reason:** ${reason}${escalation}`).setTimestamp();
      if (!isWhitelisted(member)) e.setFooter({ text: usageFooter("warn", newUsed, limit) });
      return interaction.reply({ embeds: [e] });
    }

    // ── /warnings ──────────────────────────────────────────
    case "warnings": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target = interaction.options.getUser("user");
      const list = getWarnings(guild.id, target.id);
      if (!list.length) return interaction.reply({ content: `<@${target.id}> has a clean slate - no warnings.`, ephemeral: true });
      const lines = list.slice(-15).map((w, i) =>
        `**${i + 1}.** ${w.reason} - by <@${w.by}> · <t:${Math.floor(w.at / 1000)}:R>`).join("\n");
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.warn)
        .setTitle(`⚠️ Warnings for ${target.tag}`)
        .setDescription(`**${list.length} in total.**\n\n${lines}`)
        .setFooter({ text: `Auto-actions kick in at: mute@${config.warnMuteAt} · kick@${config.warnKickAt} · ban@${config.warnBanAt}` })
        .setTimestamp()] });
    }

    // ── /clearwarns ────────────────────────────────────────
    case "clearwarns": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const target = interaction.options.getUser("user");
      const had = getWarnings(guild.id, target.id).length;
      clearWarnings(guild.id, target.id);
      secLog(guild, "Warnings Cleared", `<@${member.id}> wiped **${had}** warning${had === 1 ? "" : "s"} for <@${target.id}>.`, COLORS.success);
      return interaction.reply({ embeds: [embed(COLORS.success, `Cleared **${had}** warning${had === 1 ? "" : "s"} for <@${target.id}>. Clean slate.`, "Warnings Cleared")], ephemeral: true });
    }

    // ── /limits ────────────────────────────────────────────
    case "limits": {
      if (!isMod(member)) return interaction.reply({ content: "This one is staff only - you need the mod role.", ephemeral: true });
      const windowHours = config.modWindowMs / 3600000;
      if (isWhitelisted(member)) {
        return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
          .setTitle("🛡️ Your Mod Limits")
          .setDescription(`You're whitelisted, so none of the rate limits apply to you.`).setTimestamp()] });
      }
      const actions = [
        { key: "ban", emoji: "🔨", label: "Bans" }, { key: "kick", emoji: "👢", label: "Kicks" },
        { key: "mute", emoji: "🔇", label: "Mutes" }, { key: "warn", emoji: "⚠️", label: "Warns" },
        { key: "purge", emoji: "🗑️", label: "Purges" }, { key: "lockdown", emoji: "🔒", label: "Lockdowns" },
      ];
      const fields = actions.map(({ key, emoji, label }) => {
        const { used, limit, remaining } = checkModLimit(guild.id, member.id, key);
        const bar = buildBar(used, limit, 8);
        const pct = Math.round((used / limit) * 100);
        const warn = remaining === 0 ? " 🚫" : remaining <= Math.ceil(limit * 0.2) ? " ⚠️" : "";
        return { name: `${emoji} ${label}${warn}`, value: `\`${bar}\` **${used}/${limit}** used (${pct}%) - **${remaining}** remaining`, inline: false };
      });
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
        .setTitle("📊 Your Mod Action Limits")
        .setDescription(`Here's where you're at over the last **${windowHours}h**. These top back up on their own as older actions age out.`)
        .addFields(...fields).setTimestamp()] });
    }

    // ── /antiping ──────────────────────────────────────────
    case "antiping": {
      if (!isOwner(member) && member.id !== guild.ownerId) return interaction.reply({ content: "Only the bot owner or the server owner can change these settings.", ephemeral: true });
      const sub = interaction.options.getSubcommand();
      const a = ap(guild);
      switch (sub) {
        case "status":
          return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder()
            .setColor(a.enabled ? COLORS.success : COLORS.neutral).setTitle("📡 Anti-Ping - Status")
            .addFields(
              { name: "Enabled", value: a.enabled ? "✅ On" : "⛔ Off", inline: true },
              { name: "Action", value: `\`${a.action}\``, inline: true },
              { name: "Duration", value: `${a.timeoutMin} min`, inline: true },
              { name: "Delete message", value: a.deleteMessage ? "Yes" : "No", inline: true },
              { name: "Ignore replies", value: a.ignoreReplies ? "Yes" : "No", inline: true },
              { name: "Channel notice", value: a.notifyChannel ? "On" : "Off", inline: true },
              { name: "Response", value: `\`\`\`${a.responseTemplate}\`\`\``, inline: false },
              { name: "Protected users", value: a.protectedUsers.length ? a.protectedUsers.map(id => `<@${id}>`).join(", ") : "None", inline: false },
              { name: "Protected roles", value: a.protectedRoles.length ? a.protectedRoles.map(id => `<@&${id}>`).join(", ") : "None", inline: false },
            ).setTimestamp()] });
        case "toggle": {
          const enabled = interaction.options.getBoolean("enabled"); setAntiPing(guild.id, { enabled });
          return interaction.reply({ ephemeral: true, embeds: [embed(enabled ? COLORS.success : COLORS.neutral, `Anti-ping is now **${enabled ? "enabled" : "disabled"}**.`, "Anti-Ping")] });
        }
        case "action": {
          const action = interaction.options.getString("type"); setAntiPing(guild.id, { action });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Punishment set to **${action}**.`, "Anti-Ping")] });
        }
        case "duration": {
          const timeoutMin = interaction.options.getInteger("minutes"); setAntiPing(guild.id, { timeoutMin });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Mute/timeout duration set to **${timeoutMin} min**.`, "Anti-Ping")] });
        }
        case "delete": {
          const deleteMessage = interaction.options.getBoolean("enabled"); setAntiPing(guild.id, { deleteMessage });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Offending messages will ${deleteMessage ? "**be deleted**" : "**not be deleted**"}.`, "Anti-Ping")] });
        }
        case "ignorereplies": {
          const ignoreReplies = interaction.options.getBoolean("enabled"); setAntiPing(guild.id, { ignoreReplies });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Reply-pings will ${ignoreReplies ? "**be ignored**" : "**be punished**"}.`, "Anti-Ping")] });
        }
        case "response": {
          const text = interaction.options.getString("text");
          const responseTemplate = text.toLowerCase() === "default" ? antiPingDefaults.responseTemplate : text;
          setAntiPing(guild.id, { responseTemplate });
          const preview = renderAntiPingResponse({ responseTemplate }, member.id, "@ProtectedUser", `timed out for ${a.timeoutMin} min`);
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info,
            `Response template updated.\n\n**Template:**\n\`\`\`${responseTemplate}\`\`\`\n**Preview:**\n${preview}\n\n_Placeholders: \`{user}\`, \`{targets}\`, \`{action}\`._`, "Anti-Ping")] });
        }
        case "notify": {
          const notifyChannel = interaction.options.getBoolean("enabled"); setAntiPing(guild.id, { notifyChannel });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, `Public channel warning is now **${notifyChannel ? "on" : "off"}**.`, "Anti-Ping")] });
        }
        case "protect": {
          const action = interaction.options.getString("action");
          const user   = interaction.options.getUser("user");
          let arr = [...a.protectedUsers];
          if (action === "add") {
            if (arr.includes(user.id)) return interaction.reply({ content: `⚠️ <@${user.id}> is already protected.`, ephemeral: true });
            arr.push(user.id);
          } else arr = arr.filter(id => id !== user.id);
          setAntiPing(guild.id, { protectedUsers: arr });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `<@${user.id}> ${action === "add" ? "is now **protected**" : "is **no longer protected**"} from pings.`, "Anti-Ping")] });
        }
        case "protectrole": {
          const action = interaction.options.getString("action");
          const role   = interaction.options.getRole("role");
          let arr = [...a.protectedRoles];
          if (action === "add") {
            if (arr.includes(role.id)) return interaction.reply({ content: `⚠️ <@&${role.id}> is already protected.`, ephemeral: true });
            arr.push(role.id);
          } else arr = arr.filter(id => id !== role.id);
          setAntiPing(guild.id, { protectedRoles: arr });
          return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `<@&${role.id}> ${action === "add" ? "is now **protected**" : "is **no longer protected**"} from pings.`, "Anti-Ping")] });
        }
        case "list":
          return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info).setTitle("📡 Anti-Ping - Protected")
            .addFields(
              { name: "Users", value: a.protectedUsers.length ? a.protectedUsers.map(id => `<@${id}>`).join("\n") : "None", inline: true },
              { name: "Roles", value: a.protectedRoles.length ? a.protectedRoles.map(id => `<@&${id}>`).join("\n") : "None", inline: true },
            ).setTimestamp()] });
      }
      return;
    }

    // ── /setup ─────────────────────────────────────────────
    case "setup": {
      if (!isOwner(member) && member.id !== guild.ownerId) return interaction.reply({ content: "Only the bot owner or the server owner can change these settings.", ephemeral: true });
      const sub = interaction.options.getSubcommand();

      if (sub === "quick") {
        await interaction.deferReply({ ephemeral: true });
        const modRoleOpt = interaction.options.getRole("mod_role");
        const { created, reused } = await quickSetupGuild(guild, modRoleOpt);
        const e = buildSetupEmbed(guild, []);
        e.setTitle(`🛡️ Guardian quick setup - ${guild.name}`);
        e.setDescription(
          (created.length ? `**Created:** ${created.join(", ")}\n` : "") +
          (reused.length ? `**Reused existing:** ${reused.join(", ")}\n` : "") +
          `\nCurrent settings:`);
        return interaction.editReply({ embeds: [e] });
      }

      if (sub === "view") {
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, [])] });
      }

      if (sub === "roles") {
        const modRole  = interaction.options.getRole("mod_role");
        const muteRole = interaction.options.getRole("mute_role");
        const changes = [];
        if (modRole)  { setGuild(guild.id, "modRoleId",  modRole.id);  changes.push(`Mod role → <@&${modRole.id}>`); }
        if (muteRole) { setGuild(guild.id, "muteRoleId", muteRole.id); changes.push(`Mute role → <@&${muteRole.id}> _(ensure it denies Send Messages)_`); }
        if (!changes.length) return interaction.reply({ content: "Give me at least one role to set.", ephemeral: true });
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, changes)] });
      }

      if (sub === "channels") {
        const logCh    = interaction.options.getChannel("log_channel");
        const alertCh  = interaction.options.getChannel("alert_channel");
        const msgLogCh = interaction.options.getChannel("msg_log_channel");
        const changes = [];
        if (logCh)    { setGuild(guild.id, "logChannelId",    logCh.id);    changes.push(`Log channel → <#${logCh.id}>`); }
        if (alertCh)  { setGuild(guild.id, "alertChannelId",  alertCh.id);  changes.push(`Alert channel → <#${alertCh.id}>`); }
        if (msgLogCh) { setGuild(guild.id, "msgLogChannelId", msgLogCh.id); changes.push(`Msg log → <#${msgLogCh.id}>`); }
        if (!changes.length) return interaction.reply({ content: "Give me at least one channel to set.", ephemeral: true });
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, changes)] });
      }

      if (sub === "whitelist") {
        const action = interaction.options.getString("action");
        const user   = interaction.options.getUser("user");
        const role   = interaction.options.getRole("role");
        if (!user && !role) return interaction.reply({ content: "Give me a user or a role.", ephemeral: true });
        const changes = [];
        if (user) {
          let arr = [...gc(guild).nukeWhitelistUserIds];
          if (action === "add" && !arr.includes(user.id)) { arr.push(user.id); changes.push(`Whitelist +user <@${user.id}>`); }
          if (action === "remove") { arr = arr.filter(x => x !== user.id); changes.push(`Whitelist −user <@${user.id}>`); }
          setGuild(guild.id, "nukeWhitelistUserIds", arr);
        }
        if (role) {
          let arr = [...gc(guild).nukeWhitelistRoleIds];
          if (action === "add" && !arr.includes(role.id)) { arr.push(role.id); changes.push(`Whitelist +role <@&${role.id}>`); }
          if (action === "remove") { arr = arr.filter(x => x !== role.id); changes.push(`Whitelist −role <@&${role.id}>`); }
          setGuild(guild.id, "nukeWhitelistRoleIds", arr);
        }
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, changes)] });
      }

      if (sub === "failsafe") {
        const action = interaction.options.getString("action");
        const role   = interaction.options.getRole("role");
        let arr = [...gc(guild).failsafeRoleIds];
        const changes = [];
        if (action === "add" && !arr.includes(role.id)) { arr.push(role.id); changes.push(`Failsafe +role <@&${role.id}>`); }
        if (action === "remove") { arr = arr.filter(x => x !== role.id); changes.push(`Failsafe −role <@&${role.id}>`); }
        setGuild(guild.id, "failsafeRoleIds", arr);
        return interaction.reply({ ephemeral: true, embeds: [buildSetupEmbed(guild, changes)] });
      }
      return;
    }

    // ── /config ────────────────────────────────────────────
    case "config": {
      if (!isOwner(member) && member.id !== guild.ownerId) return interaction.reply({ content: "Only the bot owner or the server owner can view the config.", ephemeral: true });
      const windowHours = config.modWindowMs / 3600000;
      const gcfg = gc(guild);
      const acfg = ap(guild);
      const cfgEmbed = new EmbedBuilder().setTitle("🛡️ Guardian Bot - Configuration").setColor(COLORS.info)
        .addFields(
          { name: "🔧 Infrastructure", value: "​", inline: false },
          { name: "Owner(s)",     value: [...BOT_OWNER_IDS].map(id => `<@${id}>`).join(", "), inline: true },
          { name: "Log Channel",  value: gcfg.logChannelId ? `<#${gcfg.logChannelId}>` : "❌ Not set", inline: true },
          { name: "Alert Channel",value: gcfg.alertChannelId ? `<#${gcfg.alertChannelId}>` : "(uses log)", inline: true },
          { name: "Msg Log",      value: gcfg.msgLogChannelId ? `<#${gcfg.msgLogChannelId}>` : "❌ Not set", inline: true },
          { name: "Mute Role",    value: gcfg.muteRoleId ? `<@&${gcfg.muteRoleId}>` : "❌ Not set", inline: true },
          { name: "Mod Role",     value: gcfg.modRoleId ? `<@&${gcfg.modRoleId}>` : "❌ Not set", inline: true },
          { name: "🏅 Nuke Whitelist Roles", value: gcfg.nukeWhitelistRoleIds.length ? gcfg.nukeWhitelistRoleIds.map(id => `<@&${id}>`).join(", ") : "None", inline: false },
          { name: "🏅 Nuke Whitelist Users", value: gcfg.nukeWhitelistUserIds.length ? gcfg.nukeWhitelistUserIds.map(id => `<@${id}>`).join(", ") : "None", inline: false },
          { name: "💬 Anti-Spam", value: `${config.spamThreshold} msgs / ${config.spamWindowMs}ms · mention≥${config.spamMentionLimit} · dupes≥${config.spamDuplicateLimit} · invites ${config.spamBlockInvites ? "blocked" : "allowed"} → ${config.spamMuteMin} min mute`, inline: false },
          { name: "🚪 Anti-Raid", value: `${config.raidJoinThreshold} joins / ${config.raidWindowMs}ms → ${config.raidLockdownMin} min lockdown · new-acct kick: ${config.raidKickNewOnLock ? `<${config.raidMinAccountAgeMin}m` : "off"}`, inline: false },
          { name: "📡 Anti-Ping", value: `${acfg.enabled ? "On" : "Off"} • \`${acfg.action}\` • ${acfg.timeoutMin} min • ${acfg.protectedUsers.length} users / ${acfg.protectedRoles.length} roles`, inline: false },
          { name: "💣 Anti-Nuke (fast window)", value: `Window: ${config.nukeWindowMs}ms`, inline: false },
          { name: "Chan Del/Create", value: `≥ ${config.nukeChannelThreshold} / ${config.nukeChannelCreateThresh}`, inline: true },
          { name: "Role Del/Create", value: `≥ ${config.nukeRoleThreshold} / ${config.nukeRoleCreateThresh}`, inline: true },
          { name: "Bans / Kicks", value: `≥ ${config.nukeBanThreshold} / ${config.nukeKickThreshold}`, inline: true },
          { name: "Webhooks", value: `≥ ${config.nukeWebhookThreshold}`, inline: true },
          { name: "Bot add", value: `${config.nukeBotAddAction}`, inline: true },
          { name: "⚠️ Warn Escalation", value: `mute @ ${config.warnMuteAt} (${config.warnMuteMin}m) · kick @ ${config.warnKickAt} · ban @ ${config.warnBanAt}`, inline: false },
          { name: `📊 Mod Daily Limits (${windowHours}h - whitelisted exempt)`, value: "​", inline: false },
          { name: "🔨 Bans", value: `${config.modBanLimit}`, inline: true },
          { name: "👢 Kicks", value: `${config.modKickLimit}`, inline: true },
          { name: "🔇 Mutes", value: `${config.modMuteLimit}`, inline: true },
          { name: "⚠️ Warns", value: `${config.modWarnLimit}`, inline: true },
          { name: "🗑️ Purges", value: `${config.modPurgeLimit}`, inline: true },
          { name: "🔒 Lockdowns", value: `${config.modLockdownLimit}`, inline: true },
        )
        .setFooter({ text: "Edit values in .env and restart to apply changes." }).setTimestamp();
      return interaction.reply({ embeds: [cfgEmbed], ephemeral: true });
    }

    // ── /nuketest ──────────────────────────────────────────
    case "nuketest": {
      if (!isOwner(interaction.user) && interaction.user.id !== guild.ownerId)
        return interaction.reply({ content: "This one's owner only.", ephemeral: true });
      const me = guild.members.me;
      const need = [
        ["View Audit Log", PermissionsBitField.Flags.ViewAuditLog],
        ["Ban Members", PermissionsBitField.Flags.BanMembers],
        ["Kick Members", PermissionsBitField.Flags.KickMembers],
        ["Manage Roles", PermissionsBitField.Flags.ManageRoles],
        ["Manage Channels", PermissionsBitField.Flags.ManageChannels],
        ["Moderate Members", PermissionsBitField.Flags.ModerateMembers],
      ];
      const status = need.map(([n, p]) => `${me?.permissions.has(p) ? "✅" : "❌"} ${n}`).join("\n");
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.success)
        .setTitle("✅ Anti-Nuke Active")
        .setDescription(`The unified audit-log anti-nuke engine is **online**.\n\n**My permissions:**\n${status}`)
        .setTimestamp()] });
    }

    // ── /status ────────────────────────────────────────────
    case "status": {
      if (!isOwner(interaction.user) && interaction.user.id !== guild.ownerId)
        return interaction.reply({ content: "This one's owner only.", ephemeral: true });
      const mem = process.memoryUsage();
      const lockedCount = Object.keys(lockdownState).length;
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
        .setTitle("📊 Guardian Bot - Status")
        .addFields(
          { name: "Uptime",        value: formatUptime(client.uptime ?? 0), inline: true },
          { name: "WS Ping",       value: `${client.ws.ping}ms`, inline: true },
          { name: "Shard",         value: client.shard ? `${client.shard.ids.join(",")}` : "unsharded", inline: true },
          { name: "Guilds",        value: `${client.guilds.cache.size}`, inline: true },
          { name: "Memory (RSS)",  value: `${Math.round(mem.rss / 1024 / 1024)} MB`, inline: true },
          { name: "Guilds in lockdown", value: `${lockedCount}`, inline: true },
          { name: "Node.js",       value: process.version, inline: true },
        )
        .setFooter({ text: "Use /nuketest to check my permissions in this server." })
        .setTimestamp()] });
    }

    // ── /tickets ───────────────────────────────────────────
    case "tickets": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "Only the bot owner or the server owner can set up tickets.", ephemeral: true });
      const sub = interaction.options.getSubcommand();
      const cfg = getTicketConfig(guild.id);

      if (sub === "addtype") {
        const key = interaction.options.getString("key").trim().toLowerCase().replace(/[^a-z0-9_]/g, "_").slice(0, 32);
        const label = interaction.options.getString("label").trim().slice(0, 80);
        const emoji = interaction.options.getString("emoji").trim();
        const logChannel = interaction.options.getChannel("log_channel");
        if (!key) return interaction.reply({ content: "That key is not valid.", ephemeral: true });
        const types = cfg.types.filter(t => t.key !== key);
        types.push({ key, label, emoji, logChannelId: logChannel.id });
        setTicketConfig(guild.id, { types });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success,
          `Ticket type **${label}** (\`${key}\`) → logs to <#${logChannel.id}>.\nRun \`/tickets panel\` to refresh the panel with this type.`, "Ticket Type Saved")] });
      }
      if (sub === "removetype") {
        const key = interaction.options.getString("key").trim().toLowerCase();
        const had = cfg.types.some(t => t.key === key);
        setTicketConfig(guild.id, { types: cfg.types.filter(t => t.key !== key) });
        return interaction.reply({ ephemeral: true, embeds: [embed(had ? COLORS.success : COLORS.warn,
          had ? `Removed ticket type \`${key}\`. Run \`/tickets panel\` to refresh the panel.` : `No ticket type \`${key}\` was configured.`, "Ticket Type Removed")] });
      }
      if (sub === "listtypes") {
        if (!cfg.types.length) return interaction.reply({ content: "No ticket types configured yet. Use `/tickets addtype`.", ephemeral: true });
        const lines = cfg.types.map(t => `${t.emoji || "🎫"} **${t.label}** (\`${t.key}\`) → <#${t.logChannelId}>`).join("\n");
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info, lines, "Ticket Types")] });
      }
      if (sub === "category") {
        const category = interaction.options.getChannel("category");
        setTicketConfig(guild.id, { categoryId: category.id });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `New tickets will be created under **${category.name}**.`, "Ticket Category Set")] });
      }
      if (sub === "panel") {
        if (!cfg.types.length) return interaction.reply({ content: "Set up at least one ticket type first with `/tickets addtype`.", ephemeral: true });
        const channel = interaction.options.getChannel("channel") || (cfg.panelChannelId ? guild.channels.cache.get(cfg.panelChannelId) : null);
        if (!channel) return interaction.reply({ content: "Pick a channel - there is not one set yet.", ephemeral: true });
        await interaction.deferReply({ ephemeral: true });
        const panelEmbed = buildTicketPanelEmbed(guild, cfg);
        const rows = buildTicketPanelRows(cfg);
        let posted = null;
        if (cfg.panelChannelId === channel.id && cfg.panelMessageId) {
          const existingMsg = await channel.messages.fetch(cfg.panelMessageId).catch(() => null);
          if (existingMsg) posted = await existingMsg.edit({ embeds: [panelEmbed], components: rows }).catch(() => null);
        }
        if (!posted) posted = await channel.send({ embeds: [panelEmbed], components: rows }).catch(() => null);
        if (!posted) return interaction.editReply("I could not post the panel there. Please check my permissions in that channel.");
        setTicketConfig(guild.id, { panelChannelId: channel.id, panelMessageId: posted.id });
        return interaction.editReply(`Done - the ticket panel is up in <#${channel.id}>.`);
      }
      return;
    }

    // ── /applications ──────────────────────────────────────
    case "applications": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "Only the bot owner or the server owner can set up applications.", ephemeral: true });
      const sub = interaction.options.getSubcommand();

      if (sub === "list") {
        const apps = Object.values(getApplications(guild.id));
        if (!apps.length) return interaction.reply({ content: "No applications configured. They're seeded automatically when `GUILD_ID` is set.", ephemeral: true });
        const e = new EmbedBuilder().setColor(COLORS.info).setTitle("📝 Applications").setTimestamp();
        for (const a of apps) {
          e.addFields({
            name: `${a.emoji || "📝"} ${a.label} (\`${a.key}\`) - ${a.closed ? "🔒 Closed" : "🟢 Open"}`,
            value: `Panel: ${a.panelChannelId ? `<#${a.panelChannelId}>` : "❌ not set"} · Review: ${a.reviewChannelId ? `<#${a.reviewChannelId}>` : "❌ not set"}\n` +
                   `Roles on accept: ${a.acceptedRoleIds?.length ? a.acceptedRoleIds.map(id => `<@&${id}>`).join(", ") : "none"}\n` +
                   `Questions: ${a.questions?.length || 0}`,
            inline: false,
          });
        }
        return interaction.reply({ ephemeral: true, embeds: [e] });
      }

      // open / close accept a key OR the literal "all" - handle before single-app resolution.
      if (sub === "open" || sub === "close") {
        const wantClosed = sub === "close";
        const rawKey = interaction.options.getString("key")?.trim().toLowerCase();
        await interaction.deferReply({ ephemeral: true });
        const targets = rawKey === "all"
          ? Object.values(getApplications(guild.id))
          : (getApplication(guild.id, rawKey) ? [getApplication(guild.id, rawKey)] : []);
        if (!targets.length)
          return interaction.editReply(`There is no application with the key \`${rawKey}\`. Run \`/applications list\` to see the valid keys (or use \`all\`).`);
        const changed = [];
        for (const a of targets) {
          setApplication(guild.id, a.key, { closed: wantClosed });
          await refreshAppPanel(guild, getApplication(guild.id, a.key)).catch(() => {});
          changed.push(a.label);
        }
        secLog(guild, wantClosed ? "Applications Closed" : "Applications Opened",
          `<@${member.id}> ${wantClosed ? "closed" : "opened"} application(s): ${changed.join(", ")}`, wantClosed ? COLORS.neutral : COLORS.success);
        return interaction.editReply({ embeds: [embed(wantClosed ? COLORS.neutral : COLORS.success,
          `${wantClosed ? "🔒 Closed" : "🟢 Opened"} **${changed.length}** application(s): ${changed.join(", ")}.\nThe panel button${changed.length === 1 ? " has" : "s have"} been updated.`, "Applications")] });
      }

      const key = interaction.options.getString("key")?.trim().toLowerCase();
      const app = getApplication(guild.id, key);
      if (!app) return interaction.reply({ content: `There is no application with the key \`${key}\`. Run \`/applications list\` to see the valid keys.`, ephemeral: true });

      if (sub === "panel") {
        const channelOpt = interaction.options.getChannel("channel");
        await interaction.deferReply({ ephemeral: true });
        // Moving to a new channel? Re-home this app and drop its old panel message id.
        if (channelOpt && channelOpt.id !== app.panelChannelId) setApplication(guild.id, key, { panelChannelId: channelOpt.id, panelMessageId: "" });
        const channelId = channelOpt?.id || app.panelChannelId;
        if (!channelId) return interaction.editReply("Pick a channel - there is not one set for this application yet.");
        const channel = guild.channels.cache.get(channelId);
        if (!channel) return interaction.editReply("I cannot find that channel.");
        // Render the whole channel group, so a shared channel (e.g. Gambino +
        // Colombo) posts one combined panel rather than one per app.
        const apps = appsByPanelChannel(guild.id).get(channelId) || [getApplication(guild.id, key)];
        await renderChannelPanel(guild, channelId, apps);
        return interaction.editReply(`Done - the application panel (${apps.map(a => a.label).join(", ")}) is up in <#${channelId}>.`);
      }
      if (sub === "setreview") {
        const channel = interaction.options.getChannel("channel");
        setApplication(guild.id, key, { reviewChannelId: channel.id });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `**${app.label}** applications will be sent to <#${channel.id}> for review.`, "Applications")] });
      }
      if (sub === "setpanelchannel") {
        const channel = interaction.options.getChannel("channel");
        setApplication(guild.id, key, { panelChannelId: channel.id, panelMessageId: "" });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `**${app.label}** panel channel set to <#${channel.id}>. Run \`/applications panel key:${key}\` to post it.`, "Applications")] });
      }
      if (sub === "addrole") {
        const role = interaction.options.getRole("role");
        const roles = [...(app.acceptedRoleIds || [])];
        if (!roles.includes(role.id)) roles.push(role.id);
        setApplication(guild.id, key, { acceptedRoleIds: roles });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `<@&${role.id}> will be granted when a **${app.label}** application is accepted.`, "Applications")] });
      }
      if (sub === "removerole") {
        const role = interaction.options.getRole("role");
        setApplication(guild.id, key, { acceptedRoleIds: (app.acceptedRoleIds || []).filter(id => id !== role.id) });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `<@&${role.id}> removed from **${app.label}** accepted-roles.`, "Applications")] });
      }
      if (sub === "setquestions") {
        const questions = interaction.options.getString("questions").split("|").map(q => q.trim()).filter(Boolean);
        if (!questions.length) return interaction.reply({ content: "Give at least one question, separated by `|`.", ephemeral: true });
        setApplication(guild.id, key, { questions });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success,
          `**${app.label}** now has **${questions.length}** question(s):\n${questions.map((q, i) => `${i + 1}. ${q}`).join("\n")}`, "Applications")] });
      }
      return;
    }

    // ── /police manual setup ────────────────────────────────
    case "police": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "Only the bot owner or the server owner can set up the police manual.", ephemeral: true });
      const group = interaction.options.getSubcommandGroup();
      const sub = interaction.options.getSubcommand();
      if (group === "manual" && sub === "setup") {
        const channel = interaction.options.getChannel("channel") || interaction.channel;
        await interaction.deferReply({ ephemeral: true });
        const posted = await channel.send({ embeds: [buildPoliceManualEmbed()] }).catch(() => null);
        if (!posted) return interaction.editReply("I couldn't post there. Check that I have permission to send messages and embeds in that channel.");
        return interaction.editReply(`Done - the officer guide & procedures manual is up in <#${channel.id}>.`);
      }
      return;
    }

    // ── /chainofcommand ─────────────────────────────────────
    case "chainofcommand": {
      if (!isOwner(member) && member.id !== guild.ownerId)
        return interaction.reply({ content: "Only the bot owner or the server owner can set up the chain of command.", ephemeral: true });
      const sub = interaction.options.getSubcommand();

      if (sub === "list") {
        const keys = getChainKeys(guild.id);
        if (!keys.length) return interaction.reply({ content: "No chain-of-command boards configured yet.", ephemeral: true });
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info,
          keys.map(k => {
            const c = getChain(guild.id, k);
            return `\`${k}\` - ${c.channelId ? `<#${c.channelId}>` : "*(no channel set)*"} - ${c.groups.reduce((n, g) => n + (g.roleIds?.length || 0), 0)} role(s)`;
          }).join("\n"), "Chain of Command Boards")] });
      }

      const key = interaction.options.getString("key")?.trim().toLowerCase() || "default";

      if (sub === "setroles") {
        const raw = interaction.options.getString("roles");
        const roleIds = [...new Set(raw.match(/\d{15,25}/g) || [])];
        if (!roleIds.length) return interaction.reply({ content: "Give at least one role, mentioned or by ID.", ephemeral: true });
        setChain(guild.id, key, { groups: [{ label: null, roleIds }] });
        await renderChainOfCommand(guild, key);
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success,
          `Board \`${key}\` now tracks **${roleIds.length}** role(s), top rank first:\n${roleIds.map((id, i) => `${i + 1}. <@&${id}>`).join("\n")}`, "Chain of Command")] });
      }
      if (sub === "setgroup") {
        const label = interaction.options.getString("label").trim();
        const raw = interaction.options.getString("roles");
        const roleIds = [...new Set(raw.match(/\d{15,25}/g) || [])];
        if (!roleIds.length) return interaction.reply({ content: "Give at least one role, mentioned or by ID.", ephemeral: true });
        const cfg = getChain(guild.id, key);
        const groups = [...cfg.groups];
        const idx = groups.findIndex(g => (g.label || "").toLowerCase() === label.toLowerCase());
        if (idx >= 0) groups[idx] = { label, roleIds }; else groups.push({ label, roleIds });
        setChain(guild.id, key, { groups });
        await renderChainOfCommand(guild, key);
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success,
          `Board \`${key}\` group **${label}** now tracks **${roleIds.length}** role(s):\n${roleIds.map((id, i) => `${i + 1}. <@&${id}>`).join("\n")}`, "Chain of Command")] });
      }
      if (sub === "removegroup") {
        const label = interaction.options.getString("label").trim();
        const cfg = getChain(guild.id, key);
        const groups = cfg.groups.filter(g => (g.label || "").toLowerCase() !== label.toLowerCase());
        if (groups.length === cfg.groups.length) return interaction.reply({ content: `Board \`${key}\` has no group called **${label}**.`, ephemeral: true });
        setChain(guild.id, key, { groups });
        await renderChainOfCommand(guild, key);
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.success, `Removed group **${label}** from board \`${key}\`.`, "Chain of Command")] });
      }
      if (sub === "setup") {
        const cfg = getChain(guild.id, key);
        if (!cfg.groups.length) return interaction.reply({ content: `Board \`${key}\` has no roles configured yet - run \`/chainofcommand setroles\` or \`setgroup\` first.`, ephemeral: true });
        const channel = interaction.options.getChannel("channel") || interaction.channel;
        const title = interaction.options.getString("title")?.trim();
        await interaction.deferReply({ ephemeral: true });
        const patch = {};
        if (channel.id !== cfg.channelId) { patch.channelId = channel.id; patch.messageId = ""; }
        if (title) patch.title = title;
        if (Object.keys(patch).length) setChain(guild.id, key, patch);
        await renderChainOfCommand(guild, key);
        return interaction.editReply(`Done - board \`${key}\` is up in <#${channel.id}>, and will keep itself updated as roles change.`);
      }
      if (sub === "refresh") {
        const cfg = getChain(guild.id, key);
        if (!cfg.channelId || !cfg.groups.length) return interaction.reply({ content: `Board \`${key}\` isn't fully configured yet - run \`setroles\`/\`setgroup\` and \`setup\` first.`, ephemeral: true });
        await interaction.deferReply({ ephemeral: true });
        await renderChainOfCommand(guild, key);
        return interaction.editReply("Refreshed.");
      }
      if (sub === "view") {
        const cfg = getChain(guild.id, key);
        if (!cfg.groups.length) return interaction.reply({ content: `Board \`${key}\` has no roles configured yet.`, ephemeral: true });
        const body = cfg.groups.map(g => `${g.label ? `**${g.label}**\n` : ""}${(g.roleIds || []).map((id, i) => `${i + 1}. <@&${id}>`).join("\n")}`).join("\n\n");
        return interaction.reply({ ephemeral: true, embeds: [embed(COLORS.info,
          `Channel: ${cfg.channelId ? `<#${cfg.channelId}>` : "*(not set)*"}\n\n${body}`, `Chain of Command - \`${key}\``)] });
      }
      return;
    }

    // ── /help ──────────────────────────────────────────────
    case "help": {
      const windowHours = config.modWindowMs / 3600000;
      return interaction.reply({ ephemeral: true, embeds: [new EmbedBuilder().setColor(COLORS.info)
        .setTitle("🛡️ Guardian Bot - Commands")
        .addFields(
          { name: "🔇 /mute", value: "`@user [minutes] [reason]` - Mute (roles stashed & restored on unmute)", inline: false },
          { name: "🔊 /unmute", value: "`@user` - Unmute & restore stashed roles", inline: false },
          { name: "👢 /kick", value: "`@user [reason]` - Kick a member", inline: false },
          { name: "🔨 /ban", value: "`@user [reason] [delete_days]` - Ban a member", inline: false },
          { name: "♻️ /unban", value: "`user_id [reason]` - Unban by ID", inline: false },
          { name: "🗑️ /purge", value: "`count [user]` - Bulk-delete messages", inline: false },
          { name: "🔒 /lockdown", value: "`lock|unlock [channel]` - Lock or unlock a channel", inline: false },
          { name: "🚨 /panic", value: "Emergency lock **all** text channels *(owner only)*", inline: false },
          { name: "⚠️ /warn", value: "`@user [reason]` - Warn (auto-escalates to mute/kick/ban)", inline: false },
          { name: "📋 /warnings", value: "`@user` - View a member's warnings", inline: false },
          { name: "🧹 /clearwarns", value: "`@user` - Clear a member's warnings", inline: false },
          { name: "📡 /antiping", value: "Configure ping protection - `status`, `toggle`, `action`, `protect`, etc. *(bot owner only)*", inline: false },
          { name: "📊 /limits", value: "Check your remaining mod action limits today", inline: false },
          { name: "⚙️ /config", value: "View configuration *(bot owner only)*", inline: false },
          { name: "🔧 /setup", value: "`quick` auto-provisions a mute role + log channels in one step; `view`/`roles`/`channels`/`whitelist`/`failsafe` configure individual fields *(bot/server owner only)*", inline: false },
          { name: "🎫 /tickets", value: "`addtype`/`removetype`/`listtypes`/`category`/`panel` - configure the ticket system *(bot/server owner only)*", inline: false },
          { name: "📝 /applications", value: "`open`/`close` (accepts a key or `all`), `list`/`panel`/`setreview`/`setpanelchannel`/`addrole`/`removerole`/`setquestions` - configure the application system *(bot/server owner only)*", inline: false },
          { name: "👮 /police", value: "`manual setup [channel]` - post the officer guide & procedures manual *(bot/server owner only)*", inline: false },
          { name: "📋 /chainofcommand", value: "`setroles`/`setgroup`/`removegroup`/`setup [channel]`/`refresh`/`view`/`list` - auto-updating role hierarchy boards, each keyed by `key` (defaults to `default`) *(bot/server owner only)*", inline: false },
          { name: "🧪 /nuketest", value: "Confirm anti-nuke + check my permissions *(owner only)*", inline: false },
          { name: "📈 /status", value: "Bot health: uptime, latency, guild count, memory *(owner only)*", inline: false },
          { name: "⏱️ Rate Limits", value: `Mod actions are rate-limited over a **${windowHours}h** window. Use \`/limits\`.`, inline: false },
        )
        .setFooter({ text: "Guardian Bot v2 • Security Suite" }).setTimestamp()] });
    }
  }
  } catch (err) {
    console.error(`⚠️ command "${interaction.commandName}" failed:`, err);
    const msg = { content: "⚠️ Something went wrong running that command.", ephemeral: true };
    if (interaction.deferred || interaction.replied) interaction.followUp(msg).catch(() => {});
    else interaction.reply(msg).catch(() => {});
  }
});
