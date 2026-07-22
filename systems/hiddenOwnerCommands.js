// ── Hidden owner-only commands (never registered as slash → not shown in /) ──
const { Events } = require("discord.js");
const client = require("../lib/client");
const { config } = require("../lib/config");
const { appendForensic } = require("../lib/db");
const { COLORS, secLog } = require("../lib/embeds");
const { isOwner } = require("../lib/permissions");
const { runFailsafe, runRestore } = require("./failsafe");
const { snapshots, snapshotGuild, rollbackGuild } = require("./snapshotRollback");

const HIDDEN_OWNER_COMMANDS = new Set(["!failsafe", "!restore", "!snapshot", "!snapshots", "!rollback", "!ownerhelp"]);
client.on(Events.MessageCreate, async (message) => {
  if (!message.guild || !isOwner(message.author.id)) return;
  const cmd = message.content.trim().toLowerCase();
  if (!HIDDEN_OWNER_COMMANDS.has(cmd)) return;
  // Full audit trail: every invocation of a hidden owner command, regardless of outcome.
  appendForensic(message.guild.id, "owner_command", { cmd, by: message.author.id });
  try {
    if (cmd === "!failsafe") return await runFailsafe(message);
    if (cmd === "!restore")  return await runRestore(message);
    if (cmd === "!snapshot") {
      const r = await snapshotGuild(message.guild);
      const kept = (snapshots[message.guild.id] || []).length;
      secLog(message.guild, "Snapshot Taken", `<@${message.author.id}> took a manual snapshot - **${r.roles}** roles, **${r.channels}** channels.`, COLORS.success);
      return message.reply(`📸 Snapshot saved - **${r.roles}** roles, **${r.channels}** channels. (${kept}/${config.snapshotMax} kept)`);
    }
    if (cmd === "!snapshots") {
      const arr = snapshots[message.guild.id] || [];
      if (!arr.length) return message.reply("No snapshots yet. Run `!snapshot`.");
      const lines = arr.map((s, i) => `**${i + 1}.** <t:${Math.floor(s.takenAt / 1000)}:R> - ${s.roles.length} roles, ${s.channels.length} channels`).join("\n");
      return message.reply(`📸 **Snapshots (newest last):**\n${lines}`);
    }
    if (cmd === "!rollback") return await rollbackGuild(message.guild, message);
    if (cmd === "!ownerhelp") {
      return message.reply(
        "🛡️ **Hidden owner commands** (only you can run these):\n" +
        "`!failsafe` - back up + delete the target roles and kick all bots\n" +
        "`!restore` - rebuild those roles (perms, position, channel access, members)\n" +
        "`!snapshot` - take a full-guild snapshot now\n" +
        "`!snapshots` - list stored snapshots\n" +
        "`!rollback` - **destructive**: restore the server to exactly match the latest snapshot - deletes roles/channels not in it, corrects drifted permissions, re-syncs role membership. Asks for ✅ confirmation first.");
    }
  } catch (e) {
    console.error("⚠️ owner command failed:", e.message);
    message.reply(`⚠️ Command errored: ${e.message}`).catch(() => {});
  }
});
