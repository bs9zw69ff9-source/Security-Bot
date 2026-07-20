const path = require("node:path");
const os = require("node:os");
process.env.GUARDIAN_DB_FILE = path.join(os.tmpdir(), `guardian-test-permissions-${process.pid}-${Date.now()}.db`);
process.env.BOT_OWNER_IDS = "111,222";

const test = require("node:test");
const assert = require("node:assert/strict");
const bot = require("../index.js");

test("isOwner recognizes every configured owner id", () => {
  assert.equal(bot.isOwner("111"), true);
  assert.equal(bot.isOwner("222"), true);
  assert.equal(bot.isOwner("333"), false);
  assert.equal(bot.isOwner({ id: "222" }), true, "also accepts a member-like object");
});

function mockGuild(id, ownerId, botPosition = 100) {
  return { id, ownerId, members: { me: { roles: { highest: { position: botPosition } } } } };
}
function mockMember(id, guild, { position = 1, roleIds = [] } = {}) {
  return {
    id,
    guild,
    roles: {
      highest: { position },
      cache: { some: (fn) => roleIds.some(rid => fn({ id: rid })) },
    },
  };
}

test("canActOn blocks acting on the bot owner", () => {
  const guild = mockGuild("g1", "owner1");
  const target = mockMember("111", guild); // 111 is a configured owner
  const actor = mockMember("mod1", guild, { position: 50 });
  const result = bot.canActOn(actor, target);
  assert.equal(result.ok, false);
  assert.match(result.why, /bot owner/);
});

test("canActOn blocks acting on the server owner", () => {
  const guild = mockGuild("g2", "owner2");
  const target = mockMember("owner2", guild);
  const actor = mockMember("mod1", guild, { position: 50 });
  const result = bot.canActOn(actor, target);
  assert.equal(result.ok, false);
  assert.match(result.why, /server owner/);
});

test("canActOn blocks acting on a whitelisted role holder", () => {
  const guild = mockGuild("g3", "owner3");
  bot.setGuild("g3", "nukeWhitelistRoleIds", ["vip-role"]);
  const target = mockMember("u1", guild, { roleIds: ["vip-role"] });
  const actor = mockMember("mod1", guild, { position: 50 });
  const result = bot.canActOn(actor, target);
  assert.equal(result.ok, false);
  assert.match(result.why, /whitelisted/);
});

test("canActOn blocks self-action", () => {
  const guild = mockGuild("g4", "owner4");
  const actor = mockMember("mod1", guild, { position: 50 });
  const result = bot.canActOn(actor, actor);
  assert.equal(result.ok, false);
  assert.match(result.why, /yourself/);
});

test("canActOn blocks when target outranks the bot", () => {
  const guild = mockGuild("g5", "owner5", 10);
  const target = mockMember("u1", guild, { position: 20 });
  const actor = mockMember("mod1", guild, { position: 50 });
  const result = bot.canActOn(actor, target);
  assert.equal(result.ok, false);
  assert.match(result.why, /above mine/);
});

test("canActOn blocks when target outranks a non-privileged actor", () => {
  const guild = mockGuild("g6", "owner6", 100);
  const target = mockMember("u1", guild, { position: 50 });
  const actor = mockMember("mod1", guild, { position: 30 });
  const result = bot.canActOn(actor, target);
  assert.equal(result.ok, false);
  assert.match(result.why, /higher than yours/);
});

test("canActOn allows a normal, lower-ranked target", () => {
  const guild = mockGuild("g7", "owner7", 100);
  const target = mockMember("u1", guild, { position: 10 });
  const actor = mockMember("mod1", guild, { position: 30 });
  const result = bot.canActOn(actor, target);
  assert.equal(result.ok, true);
});
