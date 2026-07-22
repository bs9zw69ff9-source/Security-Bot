// ============================================================
//  GUARDIAN BOT - Discord Security Bot (multi-server)
//  v3 - SQLite persistence, global commands, shard-ready
//  Required: npm install discord.js dotenv better-sqlite3
//  Optional (scale >2500 servers): run `node shard.js` instead of `node index.js`
//
//  Entry point / orchestrator: wires up every module (see lib/, state/,
//  systems/, commands/), handles boot + a few events that don't belong to
//  any one feature, and re-exports the pure/state-only helpers the test
//  suite exercises directly.
// ============================================================

const { Events, PermissionsBitField, ActivityType, REST, Routes } = require("discord.js");
const client = require("./lib/client");
const { config, BOT_OWNER_IDS, TOKEN, CLIENT_ID } = require("./lib/config");
const { db } = require("./lib/db");
const { COLORS, alertOwner, buildBar, usageFooter, renderAntiPingResponse } = require("./lib/embeds");
const { isOwner, canActOn } = require("./lib/permissions");

// ── State (persisted per-guild config; each require also runs that
//    module's one-time home-guild seed migrations, same as before) ──
const { gc, setGuild } = require("./state/guildSettings");
const { checkModLimit, recordModAction, pruneWindow } = require("./state/modRates");
const { isLockdown, setLockdown, clearLockdown } = require("./state/lockdown");
const { ap, setAntiPing } = require("./state/antiPing");
require("./state/mutedRoles");
require("./state/warnings");
const { getTicketConfig, setTicketConfig, getOpenTicket, setOpenTicket, deleteOpenTicket, findOpenTicketByUser } = require("./state/tickets");
const { getApplications, getApplication, setApplication } = require("./state/applications");
require("./state/chainOfCommand");

// ── Systems (feature logic; each require also attaches that feature's own
//    client.on(...) listeners as a side effect) ──
const { snapshotGuild } = require("./systems/snapshotRollback");
const { recoverMutes, recoverLockdowns } = require("./systems/mute");
const { checkSpam } = require("./systems/antiSpam");
const { checkAntiPing } = require("./systems/antiPing");
require("./systems/antiRaid");
const { bump, resetBump, bumpStorm, nukeTracker, nukeStormTracker, pruneOld } = require("./systems/antiNuke");
const { joinTracker } = require("./systems/antiRaid");
const { spamTracker, dupeTracker } = require("./systems/antiSpam");
require("./systems/messageLogging");
require("./systems/hiddenOwnerCommands");
const { ensureTicketPanel } = require("./systems/tickets"); // also attaches its own button/modal listeners
const { ensureApplicationPanels } = require("./systems/applications"); // also attaches its own button/modal listeners
const { renderAllChainsOfCommand } = require("./systems/chainOfCommand"); // also attaches its own role-change listeners

// ── Commands (slash-command definitions, registration, and the big
//    InteractionCreate switch - requiring the handler attaches it) ──
const { registerCommandsGlobal, clearStaleGuildCommands } = require("./commands/register");
require("./commands/handler");

// ── Messages: anti-spam + anti-ping ───────────────────────────
client.on(Events.MessageCreate, (message) => {
  if (message.author.bot || !message.guild) return;
  if (checkSpam(message)) return;
  checkAntiPing(message);
});

// ── Boot ──────────────────────────────────────────────────────
client.once(Events.ClientReady, async () => {
  console.log(`✅ Guardian Bot online as ${client.user.tag}`);
  console.log(`👑 Owner(s): ${[...BOT_OWNER_IDS].join(", ")}`);
  client.user.setActivity("Protecting the server 🛡️", { type: ActivityType.Watching });
  if (!client.shard || client.shard.ids.includes(0)) await registerCommandsGlobal();
  await clearStaleGuildCommands(); // per-shard: only this shard's own cached guilds
  await recoverMutes();
  await recoverLockdowns();

  // Post any configured ticket + application panels that aren't already up (idempotent),
  // and refresh the chain-of-command embed in case roles changed while offline.
  for (const guild of client.guilds.cache.values()) {
    try { await ensureTicketPanel(guild); } catch (_) {}
    try { await ensureApplicationPanels(guild); } catch (_) {}
    try { await renderAllChainsOfCommand(guild); } catch (_) {}
  }

  // Permission self-audit
  for (const guild of client.guilds.cache.values()) {
    const me = guild.members.me;
    if (!me) continue;
    const missing = [];
    if (!me.permissions.has(PermissionsBitField.Flags.ViewAuditLog)) missing.push("View Audit Log (anti-nuke blind without this!)");
    if (!me.permissions.has(PermissionsBitField.Flags.BanMembers))   missing.push("Ban Members");
    if (!me.permissions.has(PermissionsBitField.Flags.ManageRoles))  missing.push("Manage Roles");
    if (!me.permissions.has(PermissionsBitField.Flags.ManageChannels)) missing.push("Manage Channels");
    if (missing.length) console.warn(`⚠️ [${guild.name}] missing permissions: ${missing.join(", ")}`);
  }

  // Take an initial full-guild snapshot, then keep rolling snapshots for nuke recovery.
  for (const guild of client.guilds.cache.values()) {
    try { const r = await snapshotGuild(guild); console.log(`📸 [${guild.name}] snapshot: ${r.roles} roles, ${r.channels} channels`); } catch (_) {}
  }
  const snapTimer = setInterval(async () => {
    for (const guild of client.guilds.cache.values()) { try { await snapshotGuild(guild); } catch (_) {} }
  }, config.snapshotIntervalMs);
  if (snapTimer.unref) snapTimer.unref();
});

client.on("error", e => console.error("client error:", e));
process.on("unhandledRejection", e => console.error("unhandledRejection:", e));

// When added to a new server: snapshot it and notify owner. (Global commands
// already cover new guilds automatically - no per-guild registration needed.)
client.on(Events.GuildCreate, async (guild) => {
  console.log(`➕ Joined guild ${guild.name} (${guild.id})`);
  try { await snapshotGuild(guild); } catch (_) {}
  try { await ensureTicketPanel(guild); } catch (_) {}
  try { await ensureApplicationPanels(guild); } catch (_) {}
  try { await renderAllChainsOfCommand(guild); } catch (_) {}
  // Clear any stray guild-scoped commands (e.g. from earlier per-guild testing on
  // this server before Guardian was invited) so nothing duplicates the global set.
  try {
    const rest = new REST({ version: "10" }).setToken(TOKEN);
    const existing = await rest.get(Routes.applicationGuildCommands(CLIENT_ID, guild.id));
    if (existing.length) {
      await rest.put(Routes.applicationGuildCommands(CLIENT_ID, guild.id), { body: [] });
      console.log(`🧹 Cleared ${existing.length} stale guild-scoped command(s) in ${guild.name}`);
    }
  } catch (_) {}
  if (config.ownerDM)
    for (const id of BOT_OWNER_IDS)
      client.users.fetch(id)
        .then(u => u.send(`Just got added to **${guild.name}** (\`${guild.id}\`). To get set up fast, run \`/setup quick\` over there - it'll create a mute role and the log channels for you. Then point me at your staff role with \`/setup roles mod_role:@YourStaffRole\` and you're good.`))
        .catch(() => {});
});

// Periodic sweep: trim stale tracker entries + self-defense health check.
const healthState = new Map(); // guildId -> last-known-ok boolean
const sweep = setInterval(() => {
  const now = Date.now();
  for (const [key, arr] of spamTracker) if (!arr.length || now - arr[arr.length - 1] > config.spamWindowMs * 5) spamTracker.delete(key);
  for (const [key, d] of dupeTracker)  if (now - d.ts > config.spamWindowMs * 5) dupeTracker.delete(key);
  for (const [key, e] of nukeTracker) {
    let any = false;
    for (const k in e) { e[k] = pruneOld(e[k]); if (e[k].length) any = true; }
    if (!any) nukeTracker.delete(key);
  }
  for (const [gid, arr] of joinTracker) {
    const pruned = arr.filter(t => now - t < config.raidWindowMs);
    if (pruned.length) joinTracker.set(gid, pruned); else joinTracker.delete(gid);
  }
  for (const [gid, arr] of nukeStormTracker) {
    const pruned = arr.filter(t => now - t < config.nukeStormWindowMs);
    if (pruned.length) nukeStormTracker.set(gid, pruned); else nukeStormTracker.delete(gid);
  }
  // Self-defense: if I lose the permissions anti-nuke needs, alert the owner (once per state change).
  for (const guild of client.guilds.cache.values()) {
    const me = guild.members.me;
    if (!me) continue;
    const ok = me.permissions.has(PermissionsBitField.Flags.ViewAuditLog) &&
               me.permissions.has(PermissionsBitField.Flags.BanMembers) &&
               me.permissions.has(PermissionsBitField.Flags.ManageRoles);
    if (healthState.get(guild.id) !== false && !ok)
      alertOwner(guild, "I've lost some permissions I really need (View Audit Log, Ban Members, or Manage Roles), which means anti-nuke could be flying blind right now. Please check my role position and permissions as soon as you can.", COLORS.danger, "I Need My Permissions Back");
    healthState.set(guild.id, ok);
  }
}, 60000);
if (sweep.unref) sweep.unref();

// Graceful shutdown: flush the DB (WAL) and disconnect cleanly.
for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => {
    console.log(`\n${sig} received - shutting down…`);
    try { db.close(); } catch (_) {}
    try { client.destroy(); } catch (_) {}
    process.exit(0);
  });
}

// Only actually connect to Discord when run directly (`node index.js` / `npm start`),
// not when required by the test suite (`require("../index.js")`).
if (require.main === module) client.login(TOKEN);

// ── Exports (for the test suite - node:test in test/*.test.js) ─────────────
// Deliberately limited to pure/state-only logic that doesn't need a live
// Discord connection: config merging, rate limits, lockdown state, warn
// escalation math, embed formatting helpers. Discord-event handlers and
// anything that touches the gateway are exercised by hand against a real
// bot instead - there's no practical way to unit-test those without it.
module.exports = {
  gc, setGuild, ap, setAntiPing,
  isOwner, BOT_OWNER_IDS,
  checkModLimit, recordModAction, pruneWindow,
  bump, resetBump, bumpStorm,
  isLockdown, setLockdown, clearLockdown,
  buildBar, usageFooter, renderAntiPingResponse,
  canActOn,
  getTicketConfig, setTicketConfig,
  getOpenTicket, setOpenTicket, deleteOpenTicket, findOpenTicketByUser,
  getApplications, getApplication, setApplication,
};
