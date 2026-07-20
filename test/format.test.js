const path = require("node:path");
const os = require("node:os");
process.env.GUARDIAN_DB_FILE = path.join(os.tmpdir(), `guardian-test-format-${process.pid}-${Date.now()}.db`);

const test = require("node:test");
const assert = require("node:assert/strict");
const bot = require("../index.js");

test("buildBar renders a proportional progress bar", () => {
  assert.equal(bot.buildBar(0, 10, 10), "░░░░░░░░░░");
  assert.equal(bot.buildBar(10, 10, 10), "██████████");
  assert.equal(bot.buildBar(5, 10, 10), "█████░░░░░");
});

test("usageFooter reports the used/limit count and warns near the limit", () => {
  const footer = bot.usageFooter("ban", 9, 10);
  assert.match(footer, /9\/10/);
  assert.match(footer, /remaining today/);
});

test("renderAntiPingResponse substitutes every placeholder", () => {
  const out = bot.renderAntiPingResponse(
    { responseTemplate: "{user} pinged {targets} and was {action}" },
    "42", "@vip", "muted for 5 min",
  );
  assert.equal(out, "<@42> pinged @vip and was muted for 5 min");
});
