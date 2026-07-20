// Per-guild configuration must default sanely and never leak between guilds.
const path = require("node:path");
const os = require("node:os");
process.env.GUARDIAN_DB_FILE = path.join(os.tmpdir(), `guardian-test-config-${process.pid}-${Date.now()}.db`);

const test = require("node:test");
const assert = require("node:assert/strict");
const bot = require("../index.js");

test("gc() returns per-guild defaults for an unconfigured guild", () => {
  const cfg = bot.gc("brand-new-guild");
  assert.equal(cfg.modRoleId, "");
  assert.deepEqual(cfg.nukeWhitelistRoleIds, []);
  assert.deepEqual(cfg.failsafeRoleIds, []);
});

test("setGuild()/gc(): one guild's settings never leak into another's", () => {
  bot.setGuild("guildX", "modRoleId", "role-x");
  bot.setGuild("guildY", "modRoleId", "role-y");
  assert.equal(bot.gc("guildX").modRoleId, "role-x");
  assert.equal(bot.gc("guildY").modRoleId, "role-y");
  assert.equal(bot.gc("guildZ").modRoleId, "", "an untouched guild must keep the default");
});

test("ap()/setAntiPing(): per-guild override merges with, doesn't replace, the defaults", () => {
  bot.setAntiPing("guildW", { action: "warn" });
  const cfg = bot.ap("guildW");
  assert.equal(cfg.action, "warn");
  assert.equal(typeof cfg.timeoutMin, "number", "unset fields still fall back to the global default");
  assert.equal(bot.ap("guildV").action, "timeout", "an untouched guild keeps the default action");
});
