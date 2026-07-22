// ── Application system config (persisted to SQLite `applications`) ──
// { [guildId]: { apps: { [key]: { key,label,emoji,panelChannelId,panelMessageId,reviewChannelId,acceptedRoleIds:[],questions:[] } } } }
const { dbLoadAll, dbPut } = require("../lib/db");
const { GUILD_ID } = require("../lib/config");

let applicationConfigs = {};
function loadApplicationConfigs() { applicationConfigs = dbLoadAll("applications"); }
function saveApplicationConfig(gid) { dbPut("applications", gid, applicationConfigs[gid]); }
loadApplicationConfigs();
function getApplications(guildId) {
  const c = applicationConfigs[guildId];
  return (c && c.apps && typeof c.apps === "object") ? c.apps : {};
}
function getApplication(guildId, key) {
  return getApplications(guildId)[key] || null;
}
function setApplication(guildId, key, patch) {
  if (!applicationConfigs[guildId]) applicationConfigs[guildId] = { apps: {} };
  if (!applicationConfigs[guildId].apps) applicationConfigs[guildId].apps = {};
  const prev = applicationConfigs[guildId].apps[key] || {};
  applicationConfigs[guildId].apps[key] = { ...prev, ...patch };
  saveApplicationConfig(guildId);
}

// One-time seed: pre-configure the exact application types + panel/review
// channels + accepted roles requested for the HOME guild (GUILD_ID) only, if
// nothing's configured yet. Never overwrites an existing configuration, and
// never applies to any other guild - use `/applications` for other servers.
function migrateApplicationsToHomeGuild() {
  if (!GUILD_ID) return;
  if (Object.keys(getApplications(GUILD_ID)).length) return;
  const FAMILY_Q = () => ([
    "What's your Ingame username?",
    "What's so special about this crime family to you?",
    "Why would you be a good pick for this family?",
    "What is your current k/d (guess if not known)",
    "How long have you been playing little Italy?",
    "How active can you be on a weekly basis?",
    "Have you been in any gangs or factions in other servers? If so explain in depth what rank you achieved and why? As well, are you still in it, if not why?",
  ]);
  const apps = {
    gambino: {
      key: "gambino", label: "Gambino", emoji: "💼",
      panelChannelId: "1528798524660252814", panelMessageId: "",
      reviewChannelId: "1529100361720266803",
      acceptedRoleIds: ["1528801101003096295", "1528801216518426866", "1528802048131338330"],
      questions: FAMILY_Q(), minAge: 14, minMemberTime: "3 days",
    },
    colombo: {
      key: "colombo", label: "Colombo", emoji: "🕴️",
      panelChannelId: "1528798524660252814", panelMessageId: "",
      reviewChannelId: "1528805634995261520",
      acceptedRoleIds: ["1528801101003096295", "1528802048131338330", "1528801296411394148"],
      questions: FAMILY_Q(), minAge: 14, minMemberTime: "3 days",
    },
    staff: {
      key: "staff", label: "Staff", emoji: "🛡️",
      panelChannelId: "1528754443129196747", panelMessageId: "",
      reviewChannelId: "1528754486678392875",
      acceptedRoleIds: ["1528754350963556466"],
      questions: [
        "DOB",
        "IGN",
        "Do you have any previous experience and how did you learn from that",
        "Why do you wish to join",
        "How will you make a meaningful impact to the community",
        "How are you better than other applicants",
      ],
      minAge: 15, minMemberTime: "2 weeks",
    },
    nypd: {
      key: "nypd", label: "NYPD", emoji: "👮",
      panelChannelId: "1528754445968740472", panelMessageId: "",
      reviewChannelId: "1528754488339464192",
      acceptedRoleIds: ["1528754363726827572", "1528754358697853050", "1528754369019777034"],
      questions: [
        "How old are you?",
        "Whats your discord and ingame name",
        "Why do you want to join the NYPD?",
        "How will you help?",
        "What would you do if someone is robbing a gun store?",
        "A higher up is giving an unlawful order, what will you do?",
      ],
      minAge: 14, minMemberTime: "1 week",
    },
  };
  applicationConfigs[GUILD_ID] = { apps, reqDefaultsV1: true };
  saveApplicationConfig(GUILD_ID);
  console.log(`📝 Seeded default application types (gambino, colombo, staff, nypd) for home guild (${GUILD_ID})`);
}
migrateApplicationsToHomeGuild();

// Backfill the per-application age / member-time requirements onto the home
// guild's already-seeded apps (added after the initial seed). Runs once,
// guarded by reqDefaultsV1, so it never clobbers later manual edits.
function migrateApplicationRequirements() {
  if (!GUILD_ID) return;
  const cfg = applicationConfigs[GUILD_ID];
  if (!cfg || !cfg.apps || cfg.reqDefaultsV1) return;
  const desired = {
    gambino: { minAge: 14, minMemberTime: "3 days" },
    colombo: { minAge: 14, minMemberTime: "3 days" },
    staff:   { minAge: 15, minMemberTime: "2 weeks" },
    nypd:    { minAge: 14, minMemberTime: "1 week" },
  };
  for (const [key, req] of Object.entries(desired)) if (cfg.apps[key]) Object.assign(cfg.apps[key], req);
  cfg.reqDefaultsV1 = true;
  saveApplicationConfig(GUILD_ID);
  console.log(`📝 Applied per-application requirements (staff 15/2wk, family 14/3d, nypd 14/1wk) for home guild (${GUILD_ID})`);
}
migrateApplicationRequirements();

// Backfill the new staff application questions onto the home guild's
// already-seeded staff app. Runs once, guarded by staffQuestionsV2, so it
// never clobbers a later manual edit via /applications setquestions.
function migrateStaffQuestionsV2() {
  if (!GUILD_ID) return;
  const cfg = applicationConfigs[GUILD_ID];
  if (!cfg || !cfg.apps || cfg.staffQuestionsV2) return;
  if (cfg.apps.staff) cfg.apps.staff.questions = [
    "DOB",
    "IGN",
    "Do you have any previous experience and how did you learn from that",
    "Why do you wish to join",
    "How will you make a meaningful impact to the community",
    "How are you better than other applicants",
  ];
  cfg.staffQuestionsV2 = true;
  saveApplicationConfig(GUILD_ID);
  console.log(`📝 Applied updated staff application questions for home guild (${GUILD_ID})`);
}
migrateStaffQuestionsV2();

// Backfill the new crime-family application questions onto the home guild's
// already-seeded gambino/colombo apps. Runs once, guarded by
// familyQuestionsV2, so it never clobbers a later manual edit via
// /applications setquestions.
function migrateFamilyQuestionsV2() {
  if (!GUILD_ID) return;
  const cfg = applicationConfigs[GUILD_ID];
  if (!cfg || !cfg.apps || cfg.familyQuestionsV2) return;
  const questions = [
    "What's your Ingame username?",
    "What's so special about this crime family to you?",
    "Why would you be a good pick for this family?",
    "What is your current k/d (guess if not known)",
    "How long have you been playing little Italy?",
    "How active can you be on a weekly basis?",
    "Have you been in any gangs or factions in other servers? If so explain in depth what rank you achieved and why? As well, are you still in it, if not why?",
  ];
  for (const key of ["gambino", "colombo"]) if (cfg.apps[key]) cfg.apps[key].questions = questions;
  cfg.familyQuestionsV2 = true;
  saveApplicationConfig(GUILD_ID);
  console.log(`📝 Applied updated crime-family application questions for home guild (${GUILD_ID})`);
}
migrateFamilyQuestionsV2();

// Backfill the new NYPD application questions onto the home guild's
// already-seeded nypd app. Runs once, guarded by nypdQuestionsV2, so it
// never clobbers a later manual edit via /applications setquestions.
function migrateNypdQuestionsV2() {
  if (!GUILD_ID) return;
  const cfg = applicationConfigs[GUILD_ID];
  if (!cfg || !cfg.apps || cfg.nypdQuestionsV2) return;
  if (cfg.apps.nypd) cfg.apps.nypd.questions = [
    "Pavlov Username:",
    "Discord Username:",
    "Age and Birthday:",
    "Time Zone:",
    "What times can you be active?",
    "Do you have previous police RP experience? If so, where?",
  ];
  cfg.nypdQuestionsV2 = true;
  saveApplicationConfig(GUILD_ID);
  console.log(`📝 Applied updated NYPD application questions for home guild (${GUILD_ID})`);
}
migrateNypdQuestionsV2();

// Backfill the expanded NYPD application questions onto the home guild's
// already-seeded nypd app. Runs once, guarded by nypdQuestionsV3, so it
// never clobbers a later manual edit via /applications setquestions.
function migrateNypdQuestionsV3() {
  if (!GUILD_ID) return;
  const cfg = applicationConfigs[GUILD_ID];
  if (!cfg || !cfg.apps || cfg.nypdQuestionsV3) return;
  if (cfg.apps.nypd) cfg.apps.nypd.questions = [
    "Pavlov Username:",
    "Discord Username:",
    "Age and Birthday:",
    "Time Zone:",
    "What times can you be active?",
    "Do you have previous police RP experience? If so, where?",
    "In your opinion, when is lethal force permitted?",
    "Scenario 1: Two gangsters are verbally fighting and are pushing towards becoming violent, what are your steps to de-escalate the situation?",
    "Scenario 2: A player kills someone, and is pleading that it was unintentional, what do you charge them with, and do you permit them to make an argument?",
    "Scenario 3: You see an officer accepting a bribe, how do you respond?",
    "Should police step into a staff situation such as RDM?",
    "Do you understand that any abuse of perms or power will lead to removal and blacklist? Y/N",
    "Do you understand you are going to likely die quite a bit? Y/N",
    "Any other questions or things you'd like to share?",
  ];
  cfg.nypdQuestionsV3 = true;
  saveApplicationConfig(GUILD_ID);
  console.log(`📝 Applied expanded NYPD application questions for home guild (${GUILD_ID})`);
}
migrateNypdQuestionsV3();

module.exports = { getApplications, getApplication, setApplication };
