// ── Chain of command config (persisted to SQLite `chain_of_command`) ──
// { [guildId]: { [key]: { channelId, messageId, title, groups: [{label, roleIds}] } } }
// A guild can have more than one board (e.g. "default" for staff, "police"
// for the department) each posted to its own channel. Within a board,
// groups are optional sub-headers (e.g. "Ranks" / "Sub Classes"); a group
// with no label just renders as a flat list. roleIds are top-rank-first.
const { dbLoadAll, dbPut } = require("../lib/db");
const { GUILD_ID } = require("../lib/config");

let chainOfCommandConfigs = {};
function loadChainOfCommandConfigs() { chainOfCommandConfigs = dbLoadAll("chain_of_command"); }
function saveChainOfCommandConfig(gid) { dbPut("chain_of_command", gid, chainOfCommandConfigs[gid]); }
loadChainOfCommandConfigs();
// Older configs stored one flat {channelId, messageId, roleIds} per guild
// (no key, no groups). Wrap that into a "default" board so already-posted
// boards keep editing the same message instead of duplicating.
function migrateChainOfCommandShape(guildId) {
  const raw = chainOfCommandConfigs[guildId];
  if (!raw || raw.default !== undefined || (!("channelId" in raw) && !("roleIds" in raw))) return;
  chainOfCommandConfigs[guildId] = { default: {
    channelId: raw.channelId || "", messageId: raw.messageId || "", title: "",
    groups: [{ label: null, roleIds: Array.isArray(raw.roleIds) ? raw.roleIds : [] }],
  } };
  saveChainOfCommandConfig(guildId);
}
for (const gid of Object.keys(chainOfCommandConfigs)) migrateChainOfCommandShape(gid);

function getChainKeys(guildId) { return Object.keys(chainOfCommandConfigs[guildId] || {}); }
function getChain(guildId, key) {
  const c = (chainOfCommandConfigs[guildId] || {})[key] || {};
  return {
    channelId: c.channelId || "", messageId: c.messageId || "",
    title: c.title || "", groups: Array.isArray(c.groups) ? c.groups : [],
  };
}
function setChain(guildId, key, patch) {
  if (!chainOfCommandConfigs[guildId]) chainOfCommandConfigs[guildId] = {};
  chainOfCommandConfigs[guildId][key] = { ...getChain(guildId, key), ...patch };
  saveChainOfCommandConfig(guildId);
}
// All role ids tracked by any board in a guild - used to decide whether a
// role change is worth reacting to at all.
function getAllChainRoleIds(guildId) {
  const chains = chainOfCommandConfigs[guildId] || {};
  return new Set(Object.values(chains).flatMap(c => (c.groups || []).flatMap(g => g.roleIds || [])));
}

// One-time seed: the requested chain-of-command role hierarchy for the HOME
// guild (GUILD_ID) only, top rank first, as the "default" board. Never
// overwrites an existing configuration - use /chainofcommand setroles for
// any other change.
function migrateChainOfCommandToHomeGuild() {
  if (!GUILD_ID) return;
  if (getChain(GUILD_ID, "default").groups.length) return;
  setChain(GUILD_ID, "default", {
    groups: [{ label: null, roleIds: [
      "1528754338472792085",
      "1528754340964208702",
      "1529251949671743671",
      "1529251385424609350",
      "1529252146925535345",
      "1529184247800266834",
      "1529184185137500192",
      "1529252370586796213",
      "1529184126358257684",
    ] }],
  });
  console.log(`📋 Seeded chain-of-command role order for home guild (${GUILD_ID})`);
}
migrateChainOfCommandToHomeGuild();

// One-time seed: the requested police chain-of-command board for the HOME
// guild (GUILD_ID) only - its own channel, and two labeled groups (Ranks,
// then Sub Classes). Never overwrites an existing configuration - use
// /chainofcommand setgroup for any other change.
function migratePoliceChainOfCommandToHomeGuild() {
  if (!GUILD_ID) return;
  if (getChain(GUILD_ID, "police").groups.length) return;
  setChain(GUILD_ID, "police", {
    channelId: "1529246137087951100",
    title: "🚓 Police Chain of Command",
    groups: [
      { label: "Ranks", roleIds: [
        "1528754354264342633",
        "1528754356063703100",
        "1528754356688781375",
        "1528754359947624639",
        "1528754360845078720",
        "1528754361906499584",
        "1528754362921254942",
        "1528754363726827572",
      ] },
      { label: "Sub Classes", roleIds: [
        "1528754365739958292",
        "1528754366851317872",
        "1528754367963074591",
      ] },
    ],
  });
  console.log(`📋 Seeded police chain-of-command board for home guild (${GUILD_ID})`);
}
migratePoliceChainOfCommandToHomeGuild();

module.exports = { getChainKeys, getChain, setChain, getAllChainRoleIds };
