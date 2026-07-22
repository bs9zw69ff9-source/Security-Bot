// ── Ticket system config (persisted to SQLite `tickets`) ──────
// { [guildId]: { panelChannelId, panelMessageId, categoryId, types: [{key,label,emoji,logChannelId}] } }
const { dbLoadAll, dbPut } = require("../lib/db");
const { GUILD_ID } = require("../lib/config");

let ticketConfigs = {};
function loadTicketConfigs() { ticketConfigs = dbLoadAll("tickets"); }
function saveTicketConfig(gid) { dbPut("tickets", gid, ticketConfigs[gid]); }
loadTicketConfigs();
function getTicketConfig(guildId) {
  const c = ticketConfigs[guildId] || {};
  return {
    panelChannelId: c.panelChannelId || "",
    panelMessageId: c.panelMessageId || "",
    categoryId: c.categoryId || "",
    types: Array.isArray(c.types) ? c.types : [],
  };
}
function setTicketConfig(guildId, patch) {
  ticketConfigs[guildId] = { ...getTicketConfig(guildId), ...patch };
  saveTicketConfig(guildId);
}

// ── Open ticket tracking (persisted to SQLite `ticket_channels`) ──
// { [guildId]: { [channelId]: { typeKey, openerId, openedAt, claimedBy, reason } } }
let ticketChannels = {};
function loadTicketChannels() { ticketChannels = dbLoadAll("ticket_channels"); }
function saveTicketChannelsFor(gid) { dbPut("ticket_channels", gid, ticketChannels[gid]); }
loadTicketChannels();
function getOpenTicket(guildId, channelId) { return ticketChannels[guildId]?.[channelId] || null; }
function setOpenTicket(guildId, channelId, data) {
  if (!ticketChannels[guildId]) ticketChannels[guildId] = {};
  ticketChannels[guildId][channelId] = data;
  saveTicketChannelsFor(guildId);
}
function deleteOpenTicket(guildId, channelId) {
  if (!ticketChannels[guildId]) return;
  delete ticketChannels[guildId][channelId];
  if (!Object.keys(ticketChannels[guildId]).length) delete ticketChannels[guildId];
  saveTicketChannelsFor(guildId);
}
function findOpenTicketByUser(guildId, userId, typeKey) {
  const chans = ticketChannels[guildId] || {};
  for (const [chId, t] of Object.entries(chans)) {
    if (t.openerId === userId && t.typeKey === typeKey) return chId;
  }
  return null;
}

// One-time seed: pre-configure the exact ticket types + panel channel requested
// for the HOME guild (GUILD_ID) only, if nothing's configured yet. Never
// overwrites an existing configuration, and never applies to any other guild -
// use `/tickets addtype` / `/tickets panel` for any other server.
function migrateTicketsToHomeGuild() {
  if (!GUILD_ID) return;
  if (getTicketConfig(GUILD_ID).types.length) return;
  setTicketConfig(GUILD_ID, {
    panelChannelId: "1528754448002711592",
    types: [
      { key: "report_player",   label: "Report Player",   emoji: "🚨", logChannelId: "1528754493536342127" },
      { key: "general_support", label: "General Support", emoji: "🎫", logChannelId: "1528754490902053034" },
      { key: "ban_appeal",      label: "Ban Appeals",     emoji: "⚖️", logChannelId: "1528754492147896500" },
      { key: "staff_report",    label: "Staff Reports",   emoji: "🛡️", logChannelId: "1528754494958080080" },
      { key: "police_report",   label: "Police Reports",  emoji: "👮", logChannelId: "1528754496392527962" },
    ],
  });
  console.log(`🎫 Seeded default ticket types + panel channel for home guild (${GUILD_ID})`);
}
migrateTicketsToHomeGuild();

module.exports = {
  getTicketConfig, setTicketConfig,
  getOpenTicket, setOpenTicket, deleteOpenTicket, findOpenTicketByUser,
};
