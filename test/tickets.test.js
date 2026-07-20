// The ticket system's config and open-ticket tracking must be isolated per
// guild, same as every other piece of per-guild state in this bot.
const path = require("node:path");
const os = require("node:os");
process.env.GUARDIAN_DB_FILE = path.join(os.tmpdir(), `guardian-test-tickets-${process.pid}-${Date.now()}.db`);

const test = require("node:test");
const assert = require("node:assert/strict");
const bot = require("../index.js");

test("getTicketConfig() returns sane defaults for an unconfigured guild", () => {
  const cfg = bot.getTicketConfig("brand-new-guild");
  assert.equal(cfg.panelChannelId, "");
  assert.equal(cfg.panelMessageId, "");
  assert.equal(cfg.categoryId, "");
  assert.deepEqual(cfg.types, []);
});

test("setTicketConfig()/getTicketConfig(): one guild's ticket config never leaks into another's", () => {
  bot.setTicketConfig("guildA", { panelChannelId: "chan-a", types: [{ key: "support", label: "Support", emoji: "🎫", logChannelId: "log-a" }] });
  bot.setTicketConfig("guildB", { panelChannelId: "chan-b", types: [] });

  assert.equal(bot.getTicketConfig("guildA").panelChannelId, "chan-a");
  assert.equal(bot.getTicketConfig("guildA").types.length, 1);
  assert.equal(bot.getTicketConfig("guildB").panelChannelId, "chan-b");
  assert.equal(bot.getTicketConfig("guildB").types.length, 0);
  assert.equal(bot.getTicketConfig("guildC").panelChannelId, "", "an untouched guild keeps the default");
});

test("setTicketConfig() merges partial patches instead of replacing the whole config", () => {
  bot.setTicketConfig("guildD", { panelChannelId: "chan-d" });
  bot.setTicketConfig("guildD", { categoryId: "cat-d" });
  const cfg = bot.getTicketConfig("guildD");
  assert.equal(cfg.panelChannelId, "chan-d", "earlier field must survive a later, unrelated patch");
  assert.equal(cfg.categoryId, "cat-d");
});

test("open-ticket tracking is isolated per guild and supports the full add/find/delete lifecycle", () => {
  assert.equal(bot.getOpenTicket("guildE", "chan-1"), null);

  bot.setOpenTicket("guildE", "chan-1", { typeKey: "support", openerId: "user1", openedAt: Date.now(), claimedBy: null, reason: "help" });
  bot.setOpenTicket("guildF", "chan-1", { typeKey: "support", openerId: "user1", openedAt: Date.now(), claimedBy: null, reason: "help" });

  assert.equal(bot.getOpenTicket("guildE", "chan-1").openerId, "user1");
  assert.equal(bot.findOpenTicketByUser("guildE", "user1", "support"), "chan-1");
  assert.equal(bot.findOpenTicketByUser("guildE", "user1", "other-type"), null, "a different ticket type must not match");
  assert.equal(bot.findOpenTicketByUser("guildF", "user2", "support"), null, "guildF's ticket must not be found under an unrelated user");

  bot.deleteOpenTicket("guildE", "chan-1");
  assert.equal(bot.getOpenTicket("guildE", "chan-1"), null);
  assert.notEqual(bot.getOpenTicket("guildF", "chan-1"), null, "deleting guildE's ticket must not affect guildF's");
});
