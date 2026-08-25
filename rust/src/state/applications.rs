//! Application system config (persisted to SQLite `applications`).
//!
//! Includes the one-time home-guild seed and every question backfill, each
//! guarded by its own flag exactly as in the original bot - so a database
//! that already ran those migrations will not have them re-applied.

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::GUILD_ID;
use crate::common::db;

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Application {
    pub key: String,
    pub label: String,
    pub emoji: String,
    pub panel_channel_id: String,
    pub panel_message_id: String,
    pub review_channel_id: String,
    /// Where a decided application is filed afterwards. Optional: leave either
    /// empty and that outcome simply isn't copied anywhere, which is how every
    /// application behaved before these existed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub accepted_channel_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub denied_channel_id: String,
    pub accepted_role_ids: Vec<String>,
    pub questions: Vec<String>,
    // Absent rather than `null`/`false` when unset, so the on-disk JSON matches
    // what the original bot wrote byte for byte and either implementation can
    // read the other's database.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_age: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_member_time: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub closed: bool,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    /// Insertion-ordered so `/applications list` reads in the seeded order.
    pub apps: IndexMap<String, Application>,
    pub req_defaults_v1: bool,
    pub staff_questions_v2: bool,
    pub family_questions_v2: bool,
    pub nypd_questions_v2: bool,
    pub nypd_questions_v3: bool,
    /// Guards the one-time NYPD review-channel correction below.
    pub nypd_review_v2: bool,
    /// Guards the one-time wasteland-faction seed below.
    pub wasteland_seed_v1: bool,
}

static CONFIGS: Lazy<Mutex<HashMap<String, AppConfig>>> = Lazy::new(|| Mutex::new(db::load_all("applications")));

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, AppConfig>> {
    match CONFIGS.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

fn save(guild_id: &str) {
    let snapshot = lock().get(guild_id).cloned();
    if let Some(s) = snapshot {
        db::put("applications", guild_id, &s);
    }
}

pub fn get_applications(guild_id: &str) -> IndexMap<String, Application> {
    lock().get(guild_id).map(|c| c.apps.clone()).unwrap_or_default()
}

pub fn get_application(guild_id: &str, key: &str) -> Option<Application> {
    lock().get(guild_id).and_then(|c| c.apps.get(key).cloned())
}

/// Merge a patch into one application (creating it if new), then persist.
pub fn update_application<F: FnOnce(&mut Application)>(guild_id: &str, key: &str, f: F) {
    {
        let mut map = lock();
        let cfg = map.entry(guild_id.to_string()).or_default();
        let app = cfg.apps.entry(key.to_string()).or_default();
        f(app);
    }
    save(guild_id);
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn family_questions() -> Vec<String> {
    strings(&[
        "What's your Ingame username?",
        "What's so special about this crime family to you?",
        "Why would you be a good pick for this family?",
        "What is your current k/d (guess if not known)",
        "How long have you been playing little Italy?",
        "How active can you be on a weekly basis?",
        "Have you been in any gangs or factions in other servers? If so explain in depth what rank you achieved and why? As well, are you still in it, if not why?",
    ])
}
fn staff_questions_v2() -> Vec<String> {
    strings(&[
        "DOB",
        "IGN",
        "Do you have any previous experience and how did you learn from that",
        "Why do you wish to join",
        "How will you make a meaningful impact to the community",
        "How are you better than other applicants",
    ])
}
fn nypd_questions_v2() -> Vec<String> {
    strings(&[
        "Pavlov Username:",
        "Discord Username:",
        "Age and Birthday:",
        "Time Zone:",
        "What times can you be active?",
        "Do you have previous police RP experience? If so, where?",
    ])
}
fn nypd_questions_v3() -> Vec<String> {
    strings(&[
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
    ])
}

/// One-time seed: pre-configure the exact application types + panel/review
/// channels + accepted roles requested for the HOME guild (GUILD_ID) only, if
/// nothing's configured yet. Never overwrites an existing configuration, and
/// never applies to any other guild - use `/applications` for other servers.
pub fn migrate_applications_to_home_guild() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    if !get_applications(home).is_empty() {
        return;
    }
    let mut apps: IndexMap<String, Application> = IndexMap::new();
    apps.insert(
        "gambino".into(),
        Application {
            key: "gambino".into(),
            label: "Gambino".into(),
            emoji: "💼".into(),
            panel_channel_id: "1528798524660252814".into(),
            panel_message_id: String::new(),
            review_channel_id: "1529100361720266803".into(),
            accepted_channel_id: String::new(),
            denied_channel_id: String::new(),
            accepted_role_ids: strings(&["1528801101003096295", "1528801216518426866", "1528802048131338330"]),
            questions: family_questions(),
            min_age: Some(14),
            min_member_time: Some("3 days".into()),
            closed: false,
        },
    );
    apps.insert(
        "colombo".into(),
        Application {
            key: "colombo".into(),
            label: "Colombo".into(),
            emoji: "🕴️".into(),
            panel_channel_id: "1528798524660252814".into(),
            panel_message_id: String::new(),
            review_channel_id: "1528805634995261520".into(),
            accepted_channel_id: String::new(),
            denied_channel_id: String::new(),
            accepted_role_ids: strings(&["1528801101003096295", "1528802048131338330", "1528801296411394148"]),
            questions: family_questions(),
            min_age: Some(14),
            min_member_time: Some("3 days".into()),
            closed: false,
        },
    );
    apps.insert(
        "staff".into(),
        Application {
            key: "staff".into(),
            label: "Staff".into(),
            emoji: "🛡️".into(),
            panel_channel_id: "1528754443129196747".into(),
            panel_message_id: String::new(),
            review_channel_id: "1528754486678392875".into(),
            accepted_channel_id: String::new(),
            denied_channel_id: String::new(),
            accepted_role_ids: strings(&["1528754350963556466"]),
            questions: staff_questions_v2(),
            min_age: Some(15),
            min_member_time: Some("2 weeks".into()),
            closed: false,
        },
    );
    apps.insert(
        "nypd".into(),
        Application {
            key: "nypd".into(),
            label: "NYPD".into(),
            emoji: "👮".into(),
            panel_channel_id: "1528754445968740472".into(),
            panel_message_id: String::new(),
            review_channel_id: "1537589602456699031".into(),
            accepted_channel_id: String::new(),
            denied_channel_id: String::new(),
            accepted_role_ids: strings(&["1528754363726827572", "1528754358697853050", "1528754369019777034"]),
            questions: strings(&[
                "How old are you?",
                "Whats your discord and ingame name",
                "Why do you want to join the NYPD?",
                "How will you help?",
                "What would you do if someone is robbing a gun store?",
                "A higher up is giving an unlawful order, what will you do?",
            ]),
            min_age: Some(14),
            min_member_time: Some("1 week".into()),
            closed: false,
        },
    );

    lock().insert(home.clone(), AppConfig { apps, req_defaults_v1: true, ..Default::default() });
    save(home);
    println!("📝 Seeded default application types (gambino, colombo, staff, nypd) for home guild ({home})");
}

/// Backfill the per-application age / member-time requirements onto the home
/// guild's already-seeded apps. Runs once, guarded by reqDefaultsV1.
pub fn migrate_application_requirements() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    {
        let mut map = lock();
        let Some(cfg) = map.get_mut(home) else { return };
        if cfg.req_defaults_v1 || cfg.apps.is_empty() {
            return;
        }
        let desired: [(&str, u32, &str); 4] = [
            ("gambino", 14, "3 days"),
            ("colombo", 14, "3 days"),
            ("staff", 15, "2 weeks"),
            ("nypd", 14, "1 week"),
        ];
        for (key, age, time) in desired {
            if let Some(app) = cfg.apps.get_mut(key) {
                app.min_age = Some(age);
                app.min_member_time = Some(time.to_string());
            }
        }
        cfg.req_defaults_v1 = true;
    }
    save(home);
    println!("📝 Applied per-application requirements (staff 15/2wk, family 14/3d, nypd 14/1wk) for home guild ({home})");
}

/// Backfill the new staff application questions. Runs once, guarded by
/// staffQuestionsV2, so it never clobbers a later `/applications setquestions`.
pub fn migrate_staff_questions_v2() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    {
        let mut map = lock();
        let Some(cfg) = map.get_mut(home) else { return };
        if cfg.staff_questions_v2 || cfg.apps.is_empty() {
            return;
        }
        if let Some(app) = cfg.apps.get_mut("staff") {
            app.questions = staff_questions_v2();
        }
        cfg.staff_questions_v2 = true;
    }
    save(home);
    println!("📝 Applied updated staff application questions for home guild ({home})");
}

/// Backfill the crime-family application questions onto gambino + colombo.
/// Runs once, guarded by familyQuestionsV2.
pub fn migrate_family_questions_v2() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    {
        let mut map = lock();
        let Some(cfg) = map.get_mut(home) else { return };
        if cfg.family_questions_v2 || cfg.apps.is_empty() {
            return;
        }
        for key in ["gambino", "colombo"] {
            if let Some(app) = cfg.apps.get_mut(key) {
                app.questions = family_questions();
            }
        }
        cfg.family_questions_v2 = true;
    }
    save(home);
    println!("📝 Applied updated crime-family application questions for home guild ({home})");
}

/// Backfill the first NYPD question set. Runs once, guarded by nypdQuestionsV2.
pub fn migrate_nypd_questions_v2() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    {
        let mut map = lock();
        let Some(cfg) = map.get_mut(home) else { return };
        if cfg.nypd_questions_v2 || cfg.apps.is_empty() {
            return;
        }
        if let Some(app) = cfg.apps.get_mut("nypd") {
            app.questions = nypd_questions_v2();
        }
        cfg.nypd_questions_v2 = true;
    }
    save(home);
    println!("📝 Applied updated NYPD application questions for home guild ({home})");
}

/// Backfill the expanded 14-question NYPD set. Runs once, guarded by
/// nypdQuestionsV3.
pub fn migrate_nypd_questions_v3() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    {
        let mut map = lock();
        let Some(cfg) = map.get_mut(home) else { return };
        if cfg.nypd_questions_v3 || cfg.apps.is_empty() {
            return;
        }
        if let Some(app) = cfg.apps.get_mut("nypd") {
            app.questions = nypd_questions_v3();
        }
        cfg.nypd_questions_v3 = true;
    }
    save(home);
    println!("📝 Applied expanded NYPD application questions for home guild ({home})");
}

/// Correct the NYPD review channel on an already-seeded home guild.
///
/// The original seed pointed at 1528754488339464192; submitted police
/// applications are meant to land in 1537589602456699031. Questions, roles and
/// panel channel are untouched - this moves the review destination only.
///
/// Runs once, guarded by nypdReviewV2, and only rewrites the channel if it is
/// still the old seeded value, so a channel an admin has since set with
/// `/applications setreview` is left alone.
pub fn migrate_nypd_review_channel_v2() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    {
        let mut map = lock();
        let Some(cfg) = map.get_mut(home) else { return };
        if cfg.nypd_review_v2 || cfg.apps.is_empty() {
            return;
        }
        if let Some(app) = cfg.apps.get_mut("nypd") {
            if app.review_channel_id == "1528754488339464192" {
                app.review_channel_id = "1537589602456699031".to_string();
            }
        }
        cfg.nypd_review_v2 = true;
    }
    save(home);
    println!("📝 Pointed NYPD application reviews at the police review channel for home guild ({home})");
}

/// The wasteland factions, for one specific server.
///
/// Unlike the other seeds this is not tied to GUILD_ID: these four belong to
/// that guild and nowhere else, so the id is the condition rather than a
/// home-guild check. Runs once, guarded by wastelandSeedV1, and never touches
/// a guild that already has these configured.
/// Set this to the server the faction applications belong to, then they are
/// seeded there on the next boot.
///
/// Empty on purpose. 1528753753186898071 was used first and turned out to be
/// the Little Italy server rather than the one the faction channels live in,
/// which put the applications under the wrong guild: the panel still posted,
/// because Discord posts by channel id without checking which server the
/// channel is in, but it wore the wrong server's name and every button was
/// dead, since the click arrived from a guild with no such application.
/// Seeding nowhere is better than seeding somewhere wrong.
const WASTELAND_GUILD: &str = "";

/// Where the faction applications were seeded by mistake, cleaned up below.
const WASTELAND_WRONG_GUILD: &str = "1528753753186898071";
const WASTELAND_KEYS: [&str; 4] = ["ncr", "legion", "bos", "enclave"];
const WASTELAND_PANEL: &str = "1541861563487494236";

/// Six of the seven questions are shared; only the faction-specific one and the
/// wording of "why" and "responsibilities" differ.
pub fn faction_questions(full_name: &str, short_name: &str, soldier: &str, situational: &str) -> Vec<String> {
    vec![
        "How old are you along with your DOB?".to_string(),
        "What is your in-game name (gamer tag)?".to_string(),
        format!("Why do you want to join {full_name}? (1 paragraph minimum)"),
        "Three enemy faction members are roaming the wasteland and you\u{2019}re by yourself. What will you do?".to_string(),
        format!("What do you think the main responsibilities of {soldier} are?"),
        situational.to_string(),
        // Short name here, not the full one: "a higher-ranking the NCR member"
        // is what happens when the article comes along for the ride.
        format!("You are ordered by a higher-ranking {short_name} member to do something you disagree with. What would you do?"),
    ]
}

/// Take the faction applications back out of the server they were filed under
/// by mistake, leaving that server's own applications untouched.
pub fn migrate_unseed_wasteland_from_wrong_guild() {
    let changed = {
        let mut map = lock();
        let Some(cfg) = map.get_mut(WASTELAND_WRONG_GUILD) else { return };
        let before = cfg.apps.len();
        cfg.apps.retain(|k, _| !WASTELAND_KEYS.contains(&k.as_str()));
        // Clear the flag too, so the seed can run properly once it has a home.
        cfg.wasteland_seed_v1 = false;
        before != cfg.apps.len()
    };
    if changed {
        save(WASTELAND_WRONG_GUILD);
        println!("📝 Removed the faction applications from {WASTELAND_WRONG_GUILD}, which was the wrong server for them");
    }
}

pub fn migrate_wasteland_applications() {
    // No home yet, so nothing to seed. See WASTELAND_GUILD.
    if WASTELAND_GUILD.is_empty() {
        return;
    }
    {
        let map = lock();
        if map.get(WASTELAND_GUILD).map(|c| c.wasteland_seed_v1).unwrap_or(false) {
            return;
        }
    }

    struct Faction {
        key: &'static str,
        label: &'static str,
        emoji: &'static str,
        /// How the faction is named where the sentence takes an article
        /// ("join the NCR", "join Caesar's Legion").
        full_name: &'static str,
        /// Bare name, for where an article would be wrong
        /// ("a higher-ranking NCR member").
        short_name: &'static str,
        /// "an NCR soldier", "a Legion soldier", and so on.
        soldier: &'static str,
        /// The one question that differs between factions.
        situational: &'static str,
        pending: &'static str,
        accepted: &'static str,
        denied: &'static str,
        roles: &'static [&'static str],
    }

    let factions = [
        Faction {
            key: "ncr",
            short_name: "NCR",
            label: "NCR",
            emoji: "\u{2b50}",
            full_name: "the NCR",
            soldier: "an NCR soldier",
            situational: "You come across a civilian being threatened by an enemy faction member. How would you handle the situation?",
            pending: "1541862720540770324",
            accepted: "1541862746977472592",
            denied: "1541862770923016273",
            roles: &["1541755579108950117", "1541837160062390272", "1541841923554148432", "1541861000418959441"],
        },
        Faction {
            key: "legion",
            short_name: "Legion",
            label: "Caesar\u{2019}s Legion",
            emoji: "\u{1f402}",
            full_name: "Caesar\u{2019}s Legion",
            soldier: "a Legion soldier",
            situational: "You encounter a settlement that refuses to submit to the Legion. How would you handle the situation?",
            pending: "1541862581730549820",
            accepted: "1541862629533024416",
            denied: "1541862677964390580",
            roles: &["1541842189636608060", "1541837268531028088", "1541755581998833716"],
        },
        Faction {
            key: "bos",
            short_name: "Brotherhood",
            label: "Brotherhood of Steel",
            emoji: "\u{2699}\u{fe0f}",
            full_name: "the Brotherhood of Steel",
            soldier: "a Brotherhood soldier",
            situational: "You come across a civilian carrying technology that the Brotherhood considers valuable. How would you handle the situation?",
            pending: "1541863321400115240",
            accepted: "1541863365175935026",
            denied: "1541863407341281402",
            roles: &["1541755583596859392", "1541837198121242714", "1541841860472078497"],
        },
        Faction {
            key: "enclave",
            short_name: "Enclave",
            label: "Enclave",
            emoji: "\u{1f985}",
            full_name: "the Enclave",
            soldier: "an Enclave soldier",
            situational: "You come across a civilian who refuses to cooperate with the Enclave. How would you handle the situation?",
            pending: "1541863487779901530",
            accepted: "1541863532272812103",
            denied: "1541863566376960060",
            roles: &["1541755584322474035", "1541837123718615142", "1541842063555956807"],
        },
    ];

    {
        let mut map = lock();
        let cfg = map.entry(WASTELAND_GUILD.to_string()).or_default();
        for f in factions {
            if cfg.apps.contains_key(f.key) {
                continue;
            }
            cfg.apps.insert(
                f.key.to_string(),
                Application {
                    key: f.key.to_string(),
                    label: f.label.to_string(),
                    emoji: f.emoji.to_string(),
                    // All four share one panel channel, so they render as a
                    // single embed with a dropdown to choose between them.
                    panel_channel_id: WASTELAND_PANEL.to_string(),
                    panel_message_id: String::new(),
                    review_channel_id: f.pending.to_string(),
                    accepted_channel_id: f.accepted.to_string(),
                    denied_channel_id: f.denied.to_string(),
                    accepted_role_ids: f.roles.iter().map(|s| s.to_string()).collect(),
                    questions: faction_questions(f.full_name, f.short_name, f.soldier, f.situational),
                    min_age: None,
                    min_member_time: None,
                    closed: false,
                },
            );
        }
        cfg.wasteland_seed_v1 = true;
    }
    save(WASTELAND_GUILD);
    println!("\u{1f4dd} Seeded the wasteland faction applications (NCR, Legion, BOS, Enclave) for {WASTELAND_GUILD}");
}
