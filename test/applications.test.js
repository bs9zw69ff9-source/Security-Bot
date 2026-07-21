// The application system's config must be isolated per guild, same as every
// other piece of per-guild state in this bot.
const path = require("node:path");
const os = require("node:os");
process.env.GUARDIAN_DB_FILE = path.join(os.tmpdir(), `guardian-test-applications-${process.pid}-${Date.now()}.db`);

const test = require("node:test");
const assert = require("node:assert/strict");
const bot = require("../index.js");

test("getApplications() returns an empty object for an unconfigured guild", () => {
  assert.deepEqual(bot.getApplications("brand-new-guild"), {});
  assert.equal(bot.getApplication("brand-new-guild", "gambino"), null);
});

test("setApplication()/getApplication(): one guild's apps never leak into another's", () => {
  bot.setApplication("guildA", "gambino", {
    key: "gambino", label: "Gambino", emoji: "💼",
    panelChannelId: "panel-a", reviewChannelId: "review-a",
    acceptedRoleIds: ["r1", "r2"], questions: ["Q1", "Q2"],
  });
  bot.setApplication("guildB", "staff", {
    key: "staff", label: "Staff", emoji: "🛡️",
    panelChannelId: "panel-b", reviewChannelId: "review-b",
    acceptedRoleIds: ["r3"], questions: ["Q1"],
  });

  assert.equal(bot.getApplication("guildA", "gambino").reviewChannelId, "review-a");
  assert.equal(bot.getApplication("guildA", "staff"), null, "guildA has no staff app");
  assert.equal(bot.getApplication("guildB", "staff").reviewChannelId, "review-b");
  assert.equal(bot.getApplication("guildB", "gambino"), null, "guildB has no gambino app");
  assert.deepEqual(bot.getApplications("guildC"), {}, "an untouched guild has no apps");
});

test("setApplication() merges partial patches instead of replacing the whole app", () => {
  bot.setApplication("guildD", "nypd", { key: "nypd", label: "NYPD", reviewChannelId: "old-review", acceptedRoleIds: ["r1"] });
  bot.setApplication("guildD", "nypd", { reviewChannelId: "new-review" });
  const app = bot.getApplication("guildD", "nypd");
  assert.equal(app.reviewChannelId, "new-review", "patched field updates");
  assert.equal(app.label, "NYPD", "unrelated field survives the patch");
  assert.deepEqual(app.acceptedRoleIds, ["r1"], "unrelated array survives the patch");
});

test("multiple apps can coexist in one guild without clobbering each other", () => {
  bot.setApplication("guildE", "gambino", { key: "gambino", label: "Gambino", reviewChannelId: "rev-gambino" });
  bot.setApplication("guildE", "colombo", { key: "colombo", label: "Colombo", reviewChannelId: "rev-colombo" });
  assert.equal(Object.keys(bot.getApplications("guildE")).length, 2);
  assert.equal(bot.getApplication("guildE", "gambino").reviewChannelId, "rev-gambino");
  assert.equal(bot.getApplication("guildE", "colombo").reviewChannelId, "rev-colombo");
});

test("open/close state: defaults to open, toggles independently, preserves other fields", () => {
  bot.setApplication("guildF", "staff", { key: "staff", label: "Staff", reviewChannelId: "rev", questions: ["Q1"] });
  // Undefined `closed` means open by default (backward-compatible with seeded apps).
  assert.ok(!bot.getApplication("guildF", "staff").closed, "a newly configured app is open by default");

  bot.setApplication("guildF", "staff", { closed: true });
  assert.equal(bot.getApplication("guildF", "staff").closed, true);
  assert.equal(bot.getApplication("guildF", "staff").reviewChannelId, "rev", "closing must not wipe other fields");
  assert.deepEqual(bot.getApplication("guildF", "staff").questions, ["Q1"]);

  bot.setApplication("guildF", "staff", { closed: false });
  assert.equal(bot.getApplication("guildF", "staff").closed, false);
});

test("open/close is isolated per app and per guild", () => {
  bot.setApplication("guildG", "gambino", { key: "gambino", label: "Gambino" });
  bot.setApplication("guildG", "colombo", { key: "colombo", label: "Colombo" });
  bot.setApplication("guildH", "gambino", { key: "gambino", label: "Gambino" });

  bot.setApplication("guildG", "gambino", { closed: true });
  assert.equal(bot.getApplication("guildG", "gambino").closed, true);
  assert.ok(!bot.getApplication("guildG", "colombo").closed, "closing gambino must not close colombo in the same guild");
  assert.ok(!bot.getApplication("guildH", "gambino").closed, "closing gambino in guildG must not affect guildH's gambino");
});
