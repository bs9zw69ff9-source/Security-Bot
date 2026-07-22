// ── Deleted-message + image logging ───────────────────────────
const { Events, EmbedBuilder } = require("discord.js");
const client = require("../lib/client");
const { gc } = require("../state/guildSettings");
const { COLORS } = require("../lib/embeds");

client.on(Events.MessageDelete, async (message) => {
  try {
    if (!message.guild) return;
    const msgLogId = gc(message.guild).msgLogChannelId;
    if (!msgLogId) return;
    if (message.channelId === msgLogId) return;                      // don't log the log channel itself
    if (message.author?.id === client.user.id) return;               // skip my own messages
    const logCh = message.guild.channels.cache.get(msgLogId);
    if (!logCh) return;

    const author = message.author;
    const desc =
      `🗑️ **Message deleted** in <#${message.channelId}>\n` +
      (author ? `**Author:** <@${author.id}> · \`${author.tag}\` · \`${author.id}\`\n` : `**Author:** _uncached_\n`) +
      (message.content ? `**Content:**\n${message.content.slice(0, 1800)}`
        : (message.partial ? "_content not cached (sent before restart)_" : "_no text content_"));

    const e = new EmbedBuilder().setColor(COLORS.muted).setDescription(desc).setTimestamp();
    if (author) e.setAuthor({ name: author.tag, iconURL: author.displayAvatarURL?.() });

    // Re-upload attachments so images survive Discord's CDN expiry.
    const files = []; const lines = []; let firstImage = null; let idx = 0;
    if (message.attachments?.size) {
      for (const att of message.attachments.values()) {
        const safe = `${idx++}_${(att.name || "file").replace(/[^a-zA-Z0-9._-]/g, "_")}`;
        files.push({ attachment: att.url, name: safe });
        lines.push(`${att.name || safe} · ${Math.round((att.size || 0) / 1024)} KB`);
        if (!firstImage && ((att.contentType || "").startsWith("image/") || /\.(png|jpe?g|gif|webp)$/i.test(att.name || "")))
          firstImage = safe;
      }
      if (firstImage) e.setImage(`attachment://${firstImage}`);
      e.addFields({ name: `Attachments (${message.attachments.size})`, value: lines.join("\n").slice(0, 1024) });
    }

    await logCh.send({ embeds: [e], files: files.length ? files : undefined })
      .catch(() => logCh.send({ embeds: [e.setImage(null)] }).catch(() => {})); // fallback if URLs already expired
  } catch (err) { console.error("msg-delete log error:", err.message); }
});

client.on(Events.MessageBulkDelete, async (messages, channel) => {
  try {
    if (!channel?.guild) return;
    const msgLogId = gc(channel.guild).msgLogChannelId;
    if (!msgLogId) return;
    if (channel.id === msgLogId) return;
    const logCh = channel.guild.channels.cache.get(msgLogId);
    if (!logCh) return;
    const cached = [...messages.values()].filter(m => m.author);
    const lines = cached.slice(0, 15).map(m => `<@${m.author.id}>: ${(m.content || "[embed/attachment]").slice(0, 80)}`).join("\n");
    const e = new EmbedBuilder().setColor(COLORS.warn).setTitle("🧹 Bulk delete")
      .setDescription(`**${messages.size}** messages deleted in <#${channel.id}>` +
        (lines ? `\n\n${lines}` : "") +
        (cached.length > 15 ? `\n…and ${cached.length - 15} more cached` : "")).setTimestamp();
    logCh.send({ embeds: [e] }).catch(() => {});
  } catch (err) { console.error("bulk-delete log error:", err.message); }
});

client.on(Events.MessageUpdate, async (oldMessage, newMessage) => {
  try {
    if (!newMessage.guild) return;
    const msgLogId = gc(newMessage.guild).msgLogChannelId;
    if (!msgLogId) return;
    if (newMessage.channelId === msgLogId) return;
    if (newMessage.author?.id === client.user.id) return;
    if (oldMessage.content === newMessage.content) return; // ignore embed-resolve / pin / non-content updates
    const logCh = newMessage.guild.channels.cache.get(msgLogId);
    if (!logCh) return;

    const author = newMessage.author;
    const before = oldMessage.partial ? "_not cached (sent before restart)_" : (oldMessage.content || "_empty_");
    const after  = newMessage.content || "_empty_";
    const e = new EmbedBuilder().setColor(COLORS.info)
      .setDescription(
        `✏️ **Message edited** in <#${newMessage.channelId}> · [jump](${newMessage.url})\n` +
        (author ? `**Author:** <@${author.id}> · \`${author.tag}\` · \`${author.id}\`` : ""))
      .addFields(
        { name: "Before", value: before.slice(0, 1024) },
        { name: "After",  value: after.slice(0, 1024) },
      ).setTimestamp();
    if (author) e.setAuthor({ name: author.tag, iconURL: author.displayAvatarURL?.() });
    logCh.send({ embeds: [e] }).catch(() => {});
  } catch (err) { console.error("msg-edit log error:", err.message); }
});
