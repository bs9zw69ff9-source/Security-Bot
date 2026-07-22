// ── Anti-Spam ─────────────────────────────────────────────────
const { config, INVITE_RE, SCAM_RE } = require("../lib/config");
const { isMod, isWhitelisted } = require("../lib/permissions");
const { COLORS, secLog, alertOwner } = require("../lib/embeds");
const { muteUser } = require("./mute");

const spamTracker = new Map();     // "gid:uid" -> [timestamps]
const dupeTracker = new Map();     // "gid:uid" -> { content, count, ts }

function checkSpam(message) {
  if (!message.member) return false;
  if (config.spamExemptStaff && (isMod(message.member) || isWhitelisted(message.member))) return false; // set SPAM_EXEMPT_STAFF=false to test on your own account
  const uid = message.author.id;
  const key = `${message.guild.id}:${uid}`;
  const now = Date.now();

  // Mass-mention in a single message (@everyone / @here counts as mass)
  const mentionCount = message.mentions.users.size + message.mentions.roles.size +
    (message.mentions.everyone ? config.spamMentionLimit : 0);
  if (mentionCount >= config.spamMentionLimit) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, `Anti-spam: mass mention (${mentionCount})`);
    secLog(message.guild, "Anti-Spam", `Muted <@${uid}> for mass-mentioning (${mentionCount}) in <#${message.channel.id}>.`, COLORS.warn);
    return true;
  }

  // Scam / phishing / IP-grabber links
  if (config.scamBlock && SCAM_RE.test(message.content)) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, "Anti-spam: scam/grabber link");
    alertOwner(message.guild, `Heads up - <@${uid}> dropped what looks like a **scam or grabber link** in <#${message.channel.id}>. I've deleted it and muted them.`, COLORS.danger, "Scam Link Blocked");
    return true;
  }

  // Invite-link spam
  if (config.spamBlockInvites && INVITE_RE.test(message.content) && !isMod(message.member)) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, "Anti-spam: posted invite link");
    secLog(message.guild, "Anti-Spam", `Muted <@${uid}> for posting an invite link in <#${message.channel.id}>.`, COLORS.warn);
    return true;
  }

  // Duplicate-message flood
  const dupe = dupeTracker.get(key);
  if (dupe && dupe.content === message.content && now - dupe.ts < config.spamWindowMs * 3) {
    dupe.count++; dupe.ts = now;
    if (dupe.count >= config.spamDuplicateLimit) {
      message.delete().catch(() => {});
      muteUser(message.member, config.spamMuteMin, "Anti-spam: duplicate flood");
      dupeTracker.set(key, { content: "", count: 0, ts: now });
      secLog(message.guild, "Anti-Spam", `Muted <@${uid}> for flooding the same message over and over in <#${message.channel.id}>.`, COLORS.warn);
      return true;
    }
  } else {
    dupeTracker.set(key, { content: message.content, count: 1, ts: now });
  }

  // Frequency flood
  const arr = (spamTracker.get(key) || []).filter(t => now - t < config.spamWindowMs);
  arr.push(now);
  spamTracker.set(key, arr);
  if (arr.length >= config.spamThreshold) {
    message.delete().catch(() => {});
    muteUser(message.member, config.spamMuteMin, "Anti-spam: message flood");
    spamTracker.set(key, []);
    secLog(message.guild, "Anti-Spam", `Muted <@${uid}> for flooding <#${message.channel.id}> with messages.`, COLORS.warn);
    return true;
  }
  return false;
}

module.exports = { checkSpam, spamTracker, dupeTracker };
