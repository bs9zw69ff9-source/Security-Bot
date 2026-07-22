// ── Full-guild snapshot + rollback (survive & undo a nuke) ────
const { ChannelType } = require("discord.js");
const { dbLoadAll, dbPut, importJsonIfPresent } = require("../lib/db");
const { config, SNAPSHOT_FILE } = require("../lib/config");
const { COLORS, alertOwner } = require("../lib/embeds");

let snapshots = {}; // { [guildId]: [ { takenAt, name, roles[], channels[] } ] }  (newest last)
function loadSnapshots() { importJsonIfPresent("snapshots", SNAPSHOT_FILE); snapshots = dbLoadAll("snapshots"); }
function saveSnapshots(gid) { dbPut("snapshots", gid, snapshots[gid]); }
loadSnapshots();

async function snapshotGuild(guild) {
  // Full member cache is required to capture accurate role membership (large
  // guilds don't get a complete member list from the gateway by default).
  await guild.members.fetch().catch(() => {});
  const roles = [...guild.roles.cache.values()]
    .filter(r => r.id !== guild.id && !r.managed)
    .sort((a, b) => a.position - b.position)
    .map(r => ({
      id: r.id, name: r.name, color: r.color, hoist: r.hoist, mentionable: r.mentionable,
      permissions: r.permissions.bitfield.toString(), position: r.position,
      members: r.members.map(m => m.id),
    }));
  const channels = [...guild.channels.cache.values()]
    .filter(c => !(c.isThread && c.isThread()))
    .map(c => ({
      id: c.id, name: c.name, type: c.type, parentId: c.parentId ?? null, position: c.rawPosition ?? 0,
      topic: c.topic ?? null, nsfw: c.nsfw ?? false, rateLimit: c.rateLimitPerUser ?? 0,
      bitrate: c.bitrate ?? null, userLimit: c.userLimit ?? null,
      overwrites: [...c.permissionOverwrites.cache.values()].map(o => ({
        id: o.id, type: o.type, allow: o.allow.bitfield.toString(), deny: o.deny.bitfield.toString(),
      })),
    }));
  const arr = snapshots[guild.id] || [];
  arr.push({ takenAt: Date.now(), name: guild.name, roles, channels });
  while (arr.length > config.snapshotMax) arr.shift();
  snapshots[guild.id] = arr;
  saveSnapshots(guild.id);
  return { roles: roles.length, channels: channels.length };
}

// Restore the guild to look EXACTLY like the latest snapshot: deletes anything
// not in the snapshot (roles/channels), corrects anything that drifted
// (permissions, overwrites, channel settings), re-syncs role membership to
// match exactly (adds AND removes), and recreates anything missing.
// Destructive by design - requires a ✅ confirmation before touching anything.
async function rollbackGuild(guild, message) {
  const snap = (snapshots[guild.id] || []).slice(-1)[0];
  if (!snap) { message?.reply("There is no snapshot saved yet. Take one with `!snapshot` first."); return; }

  await guild.roles.fetch().catch(() => {});
  await guild.channels.fetch().catch(() => {});
  await guild.members.fetch().catch(() => {});

  const snapRoleNames = new Set(snap.roles.map(r => r.name));
  const extraRoles = guild.roles.cache.filter(r =>
    r.id !== guild.id && !r.managed && !snapRoleNames.has(r.name));
  const snapChanKeys = new Set(snap.channels.map(c => `${c.name}::${c.type}`));
  const extraChannels = guild.channels.cache.filter(c =>
    !(c.isThread && c.isThread()) && !snapChanKeys.has(`${c.name}::${c.type}`));

  if (message) {
    const warning = await message.reply(
      `⚠️ **Full rollback to the snapshot from <t:${Math.floor(snap.takenAt / 1000)}:R>.** This will:\n` +
      `• **Delete ${extraRoles.size}** role(s) not in that snapshot\n` +
      `• **Delete ${extraChannels.size}** channel(s) not in that snapshot\n` +
      `• Correct permissions/overwrites on everything else to match exactly\n` +
      `• Re-sync role membership to match the snapshot (adds **and** removes members)\n\n` +
      `Anything created since the snapshot was taken - legitimate or not - will be deleted. ` +
      `React with ✅ within 30s to confirm, or ignore to cancel.`
    ).catch(() => null);
    if (!warning) return;
    await warning.react("✅").catch(() => {});
    const collected = await warning.awaitReactions({
      filter: (reaction, user) => reaction.emoji.name === "✅" && user.id === message.author.id,
      max: 1, time: 30000,
    }).catch(() => null);
    if (!collected || !collected.size) {
      await message.reply("Rollback cancelled - I did not get a confirmation in time.").catch(() => {});
      return;
    }
  }

  message?.reply(`♻️ **Rolling back** to snapshot from <t:${Math.floor(snap.takenAt / 1000)}:R> - deleting extras, correcting drift, recreating missing…`).catch(() => {});

  // 1) Delete anything not in the snapshot. Channels before categories so an
  //    emptied category isn't left behind pointlessly (not required, just tidy).
  let rolesDeleted = 0;
  for (const role of extraRoles.values()) {
    if (!role.editable) continue;
    const ok = await role.delete("Rollback: not in snapshot").then(() => true).catch(() => false);
    if (ok) rolesDeleted++;
  }
  let chansDeleted = 0;
  const extraOrdered = [
    ...extraChannels.filter(c => c.type !== ChannelType.GuildCategory).values(),
    ...extraChannels.filter(c => c.type === ChannelType.GuildCategory).values(),
  ];
  for (const ch of extraOrdered) {
    const ok = await ch.delete("Rollback: not in snapshot").then(() => true).catch(() => false);
    if (ok) chansDeleted++;
  }

  // 2) Roles: correct existing ones (matched by name) to match exactly; create missing ones.
  const roleMap = {}; let rolesCreated = 0, rolesCorrected = 0;
  for (const sr of [...snap.roles].sort((a, b) => a.position - b.position)) {
    let live = guild.roles.cache.find(r => r.name === sr.name && !r.managed && r.id !== guild.id);
    const props = { name: sr.name, color: sr.color, hoist: sr.hoist, mentionable: sr.mentionable, permissions: BigInt(sr.permissions) };
    if (live) {
      const ok = await live.edit({ ...props, reason: "Rollback: correct drifted role" }).then(() => true).catch(() => false);
      if (ok) rolesCorrected++;
    } else {
      live = await guild.roles.create({ ...props, reason: "Rollback: recreate role" }).catch(() => null);
      if (live) rolesCreated++;
    }
    if (live) roleMap[sr.id] = live;
  }
  const rolePos = Object.entries(roleMap).map(([oldId, role]) => ({
    role: role.id, position: snap.roles.find(r => r.id === oldId)?.position || 1,
  }));
  if (rolePos.length) await guild.roles.setPositions(rolePos).catch(() => {});

  // 2b) Re-sync role membership exactly to the snapshot: add whoever's missing,
  //     remove whoever has the role now but isn't in the snapshot's member list.
  let membersAdded = 0, membersRemoved = 0;
  for (const sr of snap.roles) {
    const live = roleMap[sr.id];
    if (!live) continue;
    const wanted = new Set(sr.members || []);
    for (const uid of wanted) {
      if (live.members.has(uid)) continue;
      const m = await guild.members.fetch(uid).catch(() => null);
      if (!m) continue;
      const ok = await m.roles.add(live, "Rollback: restore role membership").then(() => true).catch(() => false);
      if (ok) membersAdded++;
    }
    for (const m of live.members.values()) {
      if (wanted.has(m.id)) continue;
      const ok = await m.roles.remove(live, "Rollback: role membership not in snapshot").then(() => true).catch(() => false);
      if (ok) membersRemoved++;
    }
  }

  // Remap overwrite targets: @everyone (guild.id) is stable, roles remap by name, members stay.
  const remapOw = (ows) => {
    const out = [];
    for (const o of ows) {
      let id = o.id;
      if (o.type === 0) { // role overwrite
        if (id === guild.id) { /* @everyone - id stable */ }
        else if (roleMap[id]) id = roleMap[id].id;
        else continue; // references a role that no longer exists and wasn't recreated
      }
      out.push({ id, type: o.type, allow: BigInt(o.allow), deny: BigInt(o.deny) });
    }
    return out;
  };

  // 3) Channels: correct existing ones (incl. overwrites) to match exactly; create missing ones.
  //    Categories first so children can attach to a freshly created one.
  const chanMap = {}; let chansCreated = 0, chansCorrected = 0;
  const cats = snap.channels.filter(c => c.type === ChannelType.GuildCategory);
  const rest = snap.channels.filter(c => c.type !== ChannelType.GuildCategory);
  for (const c of cats) {
    let live = guild.channels.cache.find(ch => ch.type === ChannelType.GuildCategory && ch.name === c.name);
    const overwrites = remapOw(c.overwrites);
    if (live) {
      await live.permissionOverwrites.set(overwrites, "Rollback: correct category overwrites").catch(() => {});
      chansCorrected++;
    } else {
      live = await guild.channels.create({ name: c.name, type: ChannelType.GuildCategory, permissionOverwrites: overwrites, reason: "Rollback" }).catch(() => null);
      if (live) chansCreated++;
    }
    if (live) chanMap[c.id] = live;
  }
  for (const c of rest) {
    let live = guild.channels.cache.find(ch => ch.name === c.name && ch.type === c.type);
    const overwrites = remapOw(c.overwrites);
    const opts = { name: c.name, type: c.type, reason: "Rollback" };
    if (c.parentId && chanMap[c.parentId]) { opts.parent = chanMap[c.parentId].id; opts.lockPermissions = false; }
    if (c.type === ChannelType.GuildText || c.type === ChannelType.GuildAnnouncement) {
      opts.topic = c.topic || null;
      opts.nsfw = !!c.nsfw;
      opts.rateLimitPerUser = c.rateLimit || 0;
    }
    if (c.type === ChannelType.GuildVoice || c.type === ChannelType.GuildStageVoice) {
      if (c.bitrate) opts.bitrate = c.bitrate;
      opts.userLimit = c.userLimit || 0;
    }
    if (live) {
      await live.edit(opts).catch(() => {});
      await live.permissionOverwrites.set(overwrites, "Rollback: correct channel overwrites").catch(() => {});
      chansCorrected++;
    } else {
      live = await guild.channels.create({ ...opts, permissionOverwrites: overwrites }).catch(() => null);
      if (live) chansCreated++;
    }
    if (live) chanMap[c.id] = live;
  }
  // Best-effort channel ordering.
  for (const c of snap.channels) {
    const live = chanMap[c.id];
    if (live && typeof c.position === "number") live.setPosition(c.position).catch(() => {});
  }

  const report =
    `♻️ **Full rollback complete.**\n` +
    `• Roles: **${rolesCreated}** created, **${rolesCorrected}** corrected, **${rolesDeleted}** deleted (not in snapshot)\n` +
    `• Channels: **${chansCreated}** created, **${chansCorrected}** corrected, **${chansDeleted}** deleted (not in snapshot)\n` +
    `• Role membership: **${membersAdded}** added, **${membersRemoved}** removed to match the snapshot\n` +
    `_Recreated items get new Discord-assigned IDs; matched-by-name items were corrected in place._`;
  message?.reply(report).catch(() => {});
  alertOwner(guild, report, COLORS.success, "ROLLBACK");
}

module.exports = { snapshots, snapshotGuild, rollbackGuild };
