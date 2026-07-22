const path = require("path");
const { PermissionsBitField } = require("discord.js");

require("dotenv").config();

const TOKEN     = process.env.DISCORD_TOKEN;
const CLIENT_ID = process.env.CLIENT_ID;
const GUILD_ID  = process.env.GUILD_ID;

// ── OWNER(S) ───────────────────────────────────────────────────
// Always fully trusted: immune to anti-nuke, rate limits, and all guards.
// Also unlocks the hidden, non-slash owner commands (!failsafe, !restore, etc).
// BOT_OWNER_IDS (comma-separated) and/or BOT_OWNER_ID (singular, kept for
// backward compatibility) are merged into one trusted set. Falls back to the
// original hardcoded default if neither env var is set, so this still runs
// out of the box - override it for any real deployment.
const configuredOwnerIds = [
  ...(process.env.BOT_OWNER_IDS || "").split(",").map(s => s.trim()).filter(Boolean),
  ...(process.env.BOT_OWNER_ID ? [process.env.BOT_OWNER_ID.trim()] : []),
];
const BOT_OWNER_IDS = new Set(configuredOwnerIds.length ? configuredOwnerIds : ["1014251293159731310"]);

const ANTIPING_FILE = path.join(__dirname, "..", "antiping.json");
const MUTED_FILE    = path.join(__dirname, "..", "mutedroles.json");
const WARN_FILE     = path.join(__dirname, "..", "warnings.json");
const SETTINGS_FILE = path.join(__dirname, "..", "guildsettings.json");
const FAILSAFE_FILE = path.join(__dirname, "..", "failsafe_backup.json");
const SNAPSHOT_FILE = path.join(__dirname, "..", "guild_snapshot.json");

// ── Config ────────────────────────────────────────────────────
const config = {
  logChannelId:   process.env.LOG_CHANNEL_ID   || "",
  alertChannelId: process.env.ALERT_CHANNEL_ID || "", // criticals (owner gets pinged); falls back to log channel
  msgLogChannelId: process.env.MESSAGE_LOG_CHANNEL_ID || "", // deleted-message + image log
  modRoleId:      process.env.MOD_ROLE_ID      || "",
  muteRoleId:     process.env.MUTE_ROLE_ID     || "",

  nukeWhitelistRoleIds: (process.env.NUKE_WHITELIST_ROLE_IDS || "").split(",").filter(Boolean),
  nukeWhitelistUserIds: (process.env.NUKE_WHITELIST_USER_IDS || "").split(",").filter(Boolean),

  // Anti-spam
  spamThreshold:      parseInt(process.env.SPAM_THRESHOLD)      || 5,
  spamWindowMs:       parseInt(process.env.SPAM_WINDOW_MS)      || 3000,
  spamMuteMin:        parseInt(process.env.SPAM_MUTE_MIN)       || 10,
  spamMentionLimit:   parseInt(process.env.SPAM_MENTION_LIMIT)  || 6,    // mass-mention in a single message
  spamBlockInvites:   process.env.SPAM_BLOCK_INVITES !== "false",        // delete + mute on discord invite links
  spamDuplicateLimit: parseInt(process.env.SPAM_DUPLICATE_LIMIT) || 4,   // identical messages in window
  spamExemptStaff:    process.env.SPAM_EXEMPT_STAFF !== "false",         // mods/owner exempt; set false to stress-test yourself

  // Anti-raid
  raidJoinThreshold:   parseInt(process.env.RAID_JOIN_THRESHOLD)   || 10,
  raidWindowMs:        parseInt(process.env.RAID_WINDOW_MS)        || 10000,
  raidLockdownMin:     parseInt(process.env.RAID_LOCKDOWN_MIN)     || 5,
  raidKickNewOnLock:   process.env.RAID_KICK_NEW_ON_LOCK !== "false",     // kick brand-new accounts that join during lockdown
  raidMinAccountAgeMin:parseInt(process.env.RAID_MIN_ACCOUNT_AGE_MIN) || 1440, // <24h-old accounts are "new"

  // Anti-nuke (fast window)
  nukeWindowMs:             parseInt(process.env.NUKE_WINDOW_MS)             || 10000,
  nukeChannelThreshold:     parseInt(process.env.NUKE_CHANNEL_THRESHOLD)     || 3,
  nukeChannelCreateThresh:  parseInt(process.env.NUKE_CHANNEL_CREATE_THRESH) || 4,
  nukeRoleThreshold:        parseInt(process.env.NUKE_ROLE_THRESHOLD)        || 3,
  nukeRoleCreateThresh:     parseInt(process.env.NUKE_ROLE_CREATE_THRESH)    || 4,
  nukeBanThreshold:         parseInt(process.env.NUKE_BAN_THRESHOLD)         || 5,
  nukeKickThreshold:        parseInt(process.env.NUKE_KICK_THRESHOLD)        || 5,
  nukeWebhookThreshold:     parseInt(process.env.NUKE_WEBHOOK_THRESHOLD)     || 3,
  nukeMemberRoleThreshold:  parseInt(process.env.NUKE_MEMBER_ROLE_THRESH)    || 3, // dangerous-role grants
  nukeBotAddAction:         process.env.NUKE_BOT_ADD_ACTION || "kick",            // kick | alert
  nukeEmojiThreshold:       parseInt(process.env.NUKE_EMOJI_THRESHOLD)       || 5,

  // ── Nuke recovery + hardening ──
  snapshotIntervalMs: parseInt(process.env.SNAPSHOT_INTERVAL_MS) || 1800000, // auto full-guild snapshot every 30 min
  snapshotMax:        parseInt(process.env.SNAPSHOT_MAX)         || 5,        // rolling snapshots kept
  nukeStormThreshold: parseInt(process.env.NUKE_STORM_THRESHOLD) || 3,        // nuke responses within window → server-wide lockdown
  nukeStormWindowMs:  parseInt(process.env.NUKE_STORM_WINDOW_MS)  || 60000,
  scamBlock:          process.env.SCAM_BLOCK !== "false",                     // delete + mute on scam/phishing/grabber links
  ownerDM:            process.env.OWNER_DM   !== "false",                     // DM the owner on criticals (survives a nuked log channel)

  // ── Mod rate limits (24-hour rolling window, mods only - whitelisted users exempt) ──
  modBanLimit:      parseInt(process.env.MOD_BAN_LIMIT)      || 3,
  modKickLimit:     parseInt(process.env.MOD_KICK_LIMIT)     || 10,
  modMuteLimit:     parseInt(process.env.MOD_MUTE_LIMIT)     || 20,
  modPurgeLimit:    parseInt(process.env.MOD_PURGE_LIMIT)    || 5,
  modLockdownLimit: parseInt(process.env.MOD_LOCKDOWN_LIMIT) || 5,
  modWarnLimit:     parseInt(process.env.MOD_WARN_LIMIT)     || 30,
  modWindowMs:      parseInt(process.env.MOD_WINDOW_MS)      || 86400000, // 24h default

  // ── Warn escalation (0 = disabled) ──
  warnMuteAt:  parseInt(process.env.WARN_MUTE_AT)  || 3,
  warnKickAt:  parseInt(process.env.WARN_KICK_AT)  || 5,
  warnBanAt:   parseInt(process.env.WARN_BAN_AT)   || 7,
  warnMuteMin: parseInt(process.env.WARN_MUTE_MIN) || 60,

  // ── Anti-ping ──
  antiPingEnabled:       process.env.ANTIPING_ENABLED !== "false",
  antiPingAction:        process.env.ANTIPING_ACTION || "timeout", // none | warn | mute | timeout
  antiPingTimeoutMin:    parseInt(process.env.ANTIPING_TIMEOUT_MIN) || 5,
  antiPingDeleteMessage: process.env.ANTIPING_DELETE === "true",
  antiPingIgnoreReplies: process.env.ANTIPING_IGNORE_REPLIES !== "false",
  antiPingNotifyChannel: process.env.ANTIPING_NOTIFY !== "false",
  antiPingResponse:      process.env.ANTIPING_RESPONSE ||
                         "{user}, please don't ping {targets}. You have been {action}.",
  antiPingProtectedUserIds: (process.env.ANTIPING_PROTECTED_USER_IDS || "").split(",").filter(Boolean),
  antiPingProtectedRoleIds: (process.env.ANTIPING_PROTECTED_ROLE_IDS || "").split(",").filter(Boolean),
};

// Dangerous permissions used across anti-nuke checks.
const DANGER_PERMS = [
  PermissionsBitField.Flags.Administrator,
  PermissionsBitField.Flags.ManageGuild,
  PermissionsBitField.Flags.ManageChannels,
  PermissionsBitField.Flags.ManageRoles,
  PermissionsBitField.Flags.ManageWebhooks,
  PermissionsBitField.Flags.BanMembers,
  PermissionsBitField.Flags.KickMembers,
];

const INVITE_RE = /(discord\.(gg|io|me|li)|discordapp\.com\/invite|discord\.com\/invite)\/[a-z0-9-]+/i;

// High-precision scam/grabber patterns (typo-squats + known IP grabbers). Kept tight to avoid false positives.
const SCAM_RE = /(dlscord|disc0rd|discocrd|dlscordnitro|steamcommunilty|steancommunity|grabify\.link|iplogger\.(org|com|ru|co)|discordapp\.(ru|info)|free-?nitro-?gen|nitro-?free-?gift)/i;

module.exports = {
  TOKEN, CLIENT_ID, GUILD_ID, BOT_OWNER_IDS, config,
  DANGER_PERMS, INVITE_RE, SCAM_RE,
  ANTIPING_FILE, MUTED_FILE, WARN_FILE, SETTINGS_FILE, FAILSAFE_FILE, SNAPSHOT_FILE,
};
