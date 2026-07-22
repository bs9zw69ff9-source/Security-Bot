// ── Register Commands (GLOBAL - one registration serves every server, present
//    and future; propagation can take up to ~1h, like Wick/large bots) ──
const { REST, Routes } = require("discord.js");
const client = require("../lib/client");
const { TOKEN, CLIENT_ID } = require("../lib/config");
const { commandBody } = require("./definitions");

async function registerCommandsGlobal() {
  const rest = new REST({ version: "10" }).setToken(TOKEN);
  try {
    console.log("🔄 Registering global slash commands…");
    await rest.put(Routes.applicationCommands(CLIENT_ID), { body: commandBody() });
    console.log("✅ Global commands registered (available in every server; new servers may take up to ~1h).");
  } catch (e) { console.error("❌ Global command registration failed:", e.message); }
}

// Guild-scoped commands (e.g. left over from earlier testing/iteration, or a
// stray script) sit ALONGSIDE identically-named global ones and show up as
// duplicates in Discord's command picker for that server. We only ever
// register globally, so wipe any leftover per-guild commands on every guild
// we're currently in.
async function clearStaleGuildCommands() {
  const rest = new REST({ version: "10" }).setToken(TOKEN);
  for (const guild of client.guilds.cache.values()) {
    try {
      const existing = await rest.get(Routes.applicationGuildCommands(CLIENT_ID, guild.id));
      if (!existing.length) continue;
      await rest.put(Routes.applicationGuildCommands(CLIENT_ID, guild.id), { body: [] });
      console.log(`🧹 Cleared ${existing.length} stale guild-scoped command(s) in ${guild.name} (${guild.id}) - was causing duplicates.`);
    } catch (e) { console.error(`⚠️ Failed to clear guild commands for ${guild.name}:`, e.message); }
  }
}

module.exports = { registerCommandsGlobal, clearStaleGuildCommands };
