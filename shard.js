// ============================================================
//  GUARDIAN BOT — Sharding entry point
//  Use this instead of index.js once a single process can no longer
//  hold every guild (Discord requires sharding past ~2500 guilds).
//  Usage: node shard.js
// ============================================================

require("dotenv").config();
const { ShardingManager } = require("discord.js");
const path = require("path");

const TOKEN = process.env.DISCORD_TOKEN;
if (!TOKEN) {
  console.error("❌ DISCORD_TOKEN is not set.");
  process.exit(1);
}

const manager = new ShardingManager(path.join(__dirname, "index.js"), {
  token: TOKEN,
  totalShards: "auto",
});

manager.on("shardCreate", (shard) => {
  console.log(`🐚 Launched shard ${shard.id}`);
});

manager.spawn().catch((e) => {
  console.error("❌ Failed to spawn shards:", e.message);
  process.exit(1);
});
