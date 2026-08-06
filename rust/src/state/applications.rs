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
            review_channel_id: "1528754488339464192".into(),
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
