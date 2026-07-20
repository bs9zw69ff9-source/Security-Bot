// Regression tests for the cross-guild state-leak fix: every tracker below
// must be fully isolated per guild, and the persisted ones (mod rate limits,
// lockdown state) must survive a restart.
const path = require("node:path");
const os = require("node:os");
process.env.GUARDIAN_DB_FILE = path.join(os.tmpdir(), `guardian-test-multiserver-${process.pid}-${Date.now()}.db`);

const test = require("node:test");
const assert = require("node:assert/strict");
const bot = require("../index.js");

test("bump/resetBump: same user, different guilds, independent counters", () => {
  assert.equal(bot.bump("guildA", "user1", "kicks", 3), false);
  assert.equal(bot.bump("guildA", "user1", "kicks", 3), false);
  assert.equal(bot.bump("guildB", "user1", "kicks", 3), false, "guildB must not see guildA's bumps");
  assert.equal(bot.bump("guildA", "user1", "kicks", 3), true, "guildA reaches its own threshold on its 3rd bump");
  bot.resetBump("guildA", "user1", "kicks");
  assert.equal(bot.bump("guildA", "user1", "kicks", 3), false, "reset clears only guildA's count");
});

test("checkModLimit/recordModAction: same user, different guilds, independent daily limits", () => {
  for (let i = 0; i < 3; i++) bot.recordModAction("guildC", "mod1", "ban");
  const guildC = bot.checkModLimit("guildC", "mod1", "ban");
  const guildD = bot.checkModLimit("guildD", "mod1", "ban");
  assert.equal(guildC.used, 3);
  assert.equal(guildD.used, 0, "mod1's bans in guildC must not count against guildD's limit");
});

test("checkModLimit: denies once the configured limit is reached", () => {
  for (let i = 0; i < 3; i++) bot.recordModAction("guildE", "mod2", "ban"); // default modBanLimit is 3
  const status = bot.checkModLimit("guildE", "mod2", "ban");
  assert.equal(status.allowed, false);
  assert.equal(status.remaining, 0);
});

test("lockdown state is isolated per guild", () => {
  assert.equal(bot.isLockdown("guildF"), false);
  bot.setLockdown("guildF", "raid", null);
  assert.equal(bot.isLockdown("guildF"), true);
  assert.equal(bot.isLockdown("guildG"), false, "an unrelated guild must not be affected");
  bot.clearLockdown("guildF");
  assert.equal(bot.isLockdown("guildF"), false);
});

test("mod rate limits and lockdown state survive a simulated restart (persisted to SQLite)", () => {
  for (let i = 0; i < 2; i++) bot.recordModAction("guildH", "mod3", "kick");
  bot.setLockdown("guildH", "panic", null);

  // Simulate a process restart: drop the cached module and re-require it, which
  // re-runs index.js's top-level load-from-SQLite code against the same DB file.
  delete require.cache[require.resolve("../index.js")];
  const reloaded = require("../index.js");

  const status = reloaded.checkModLimit("guildH", "mod3", "kick");
  assert.equal(status.used, 2, "mod rate-limit count should survive reload from disk");
  assert.equal(reloaded.isLockdown("guildH"), true, "lockdown state should survive reload from disk");
});
