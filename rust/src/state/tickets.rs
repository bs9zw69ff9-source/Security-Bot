//! Ticket system config (`tickets`) and open-ticket tracking (`ticket_channels`).

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::GUILD_ID;
use crate::common::db;

/// One ticket type, modelled on Appy's ticket template: the type carries its
/// own support roles, category and log channel rather than the whole server
/// sharing one set.
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TicketType {
    pub key: String,
    pub label: String,
    pub emoji: String,
    pub log_channel_id: String,
    /// Who handles this type: they can see the ticket, reply, claim and close
    /// it, and they get pinged when one opens.
    ///
    /// Empty means fall back to the server's mod role, which is how every
    /// ticket behaved before this existed, so an untouched configuration keeps
    /// working exactly as it did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub support_role_ids: Vec<String>,
    /// Category this type's tickets open under. Empty falls back to the
    /// server-wide category.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub category_id: String,
    /// Panel this type appears on. Empty falls back to the server-wide panel
    /// channel, so all the types share one panel unless you split them up.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub panel_channel_id: String,
    /// Message id of the panel this type is on, tracked per panel channel.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub panel_message_id: String,
}

impl TicketType {
    /// The roles that handle this type, falling back to the server mod role.
    pub fn support_roles(&self, mod_role_id: &str) -> Vec<String> {
        if !self.support_role_ids.is_empty() {
            return self.support_role_ids.clone();
        }
        if mod_role_id.is_empty() {
            return Vec::new();
        }
        vec![mod_role_id.to_string()]
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TicketConfig {
    pub panel_channel_id: String,
    pub panel_message_id: String,
    pub category_id: String,
    pub types: Vec<TicketType>,
    /// Guards the one-time category backfill below.
    pub category_seed_v1: bool,
    /// Guards the one-time ticket seed for the wasteland server.
    pub wasteland_seed_v1: bool,
}

static CONFIGS: Lazy<Mutex<HashMap<String, TicketConfig>>> = Lazy::new(|| Mutex::new(db::load_all("tickets")));

fn lock_cfg() -> std::sync::MutexGuard<'static, HashMap<String, TicketConfig>> {
    match CONFIGS.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

pub fn get_ticket_config(guild_id: &str) -> TicketConfig {
    lock_cfg().get(guild_id).cloned().unwrap_or_default()
}

/// Returns whether the change reached the database, so a caller can avoid
/// reporting a save that did not happen.
pub fn update_ticket_config<F: FnOnce(&mut TicketConfig)>(guild_id: &str, f: F) -> bool {
    let mut map = lock_cfg();
    let entry = map.entry(guild_id.to_string()).or_default();
    f(entry);
    let snapshot = entry.clone();
    drop(map);
    db::put("tickets", guild_id, &snapshot)
}

// ── Open ticket tracking ──────────────────────────────────────
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTicket {
    pub type_key: String,
    pub opener_id: String,
    pub opened_at: i64,
    pub claimed_by: Option<String>,
    pub reason: String,
}

type Channels = HashMap<String, HashMap<String, OpenTicket>>;
static CHANNELS: Lazy<Mutex<Channels>> = Lazy::new(|| Mutex::new(db::load_all("ticket_channels")));

fn lock_ch() -> std::sync::MutexGuard<'static, Channels> {
    match CHANNELS.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

pub fn get_open_ticket(guild_id: &str, channel_id: &str) -> Option<OpenTicket> {
    lock_ch().get(guild_id).and_then(|g| g.get(channel_id)).cloned()
}

pub fn set_open_ticket(guild_id: &str, channel_id: &str, data: OpenTicket) {
    let mut map = lock_ch();
    let guild = map.entry(guild_id.to_string()).or_default();
    guild.insert(channel_id.to_string(), data);
    let snapshot = guild.clone();
    drop(map);
    db::put("ticket_channels", guild_id, &snapshot);
}

pub fn delete_open_ticket(guild_id: &str, channel_id: &str) {
    let mut map = lock_ch();
    let Some(guild) = map.get_mut(guild_id) else { return };
    guild.remove(channel_id);
    if guild.is_empty() {
        map.remove(guild_id);
        drop(map);
        db::delete("ticket_channels", guild_id);
        return;
    }
    let snapshot = guild.clone();
    drop(map);
    db::put("ticket_channels", guild_id, &snapshot);
}

pub fn find_open_ticket_by_user(guild_id: &str, user_id: &str, type_key: &str) -> Option<String> {
    lock_ch().get(guild_id).and_then(|chans| {
        chans
            .iter()
            .find(|(_, t)| t.opener_id == user_id && t.type_key == type_key)
            .map(|(ch_id, _)| ch_id.clone())
    })
}

/// One-time seed: pre-configure the exact ticket types + panel channel
/// requested for the HOME guild (GUILD_ID) only, if nothing's configured yet.
/// Never overwrites an existing configuration, and never applies to any other
/// guild - use `/tickets addtype` / `/tickets panel` for any other server.
pub fn migrate_tickets_to_home_guild() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    if !get_ticket_config(home).types.is_empty() {
        return;
    }
    let types = vec![
        ("report_player", "Report Player", "🚨", "1528754493536342127"),
        ("general_support", "General Support", "🎫", "1528754490902053034"),
        ("ban_appeal", "Ban Appeals", "⚖️", "1528754492147896500"),
        ("staff_report", "Staff Reports", "🛡️", "1528754494958080080"),
        ("police_report", "Police Reports", "👮", "1528754496392527962"),
    ];
    update_ticket_config(home, |c| {
        c.panel_channel_id = "1528754448002711592".to_string();
        c.category_id = "1534402515557417161".to_string();
        c.types = types
            .into_iter()
            .map(|(key, label, emoji, log)| TicketType {
                key: key.to_string(),
                label: label.to_string(),
                emoji: emoji.to_string(),
                log_channel_id: log.to_string(),
                ..Default::default()
            })
            .collect();
    });
    println!("🎫 Seeded default ticket types + panel channel for home guild ({home})");
}

/// Backfill the open-ticket category onto the home guild's already-seeded
/// ticket config. The original seed never set one, so guilds seeded before
/// this ran fell back to auto-creating a "Tickets" category on first use.
/// Runs once, guarded by categorySeedV1, and never overwrites a category an
/// admin has already chosen.
pub fn migrate_ticket_category() {
    let Some(home) = GUILD_ID.as_ref() else { return };
    let cfg = get_ticket_config(home);
    if cfg.category_seed_v1 || cfg.types.is_empty() {
        return;
    }
    update_ticket_config(home, |c| {
        if c.category_id.is_empty() {
            c.category_id = "1534402515557417161".to_string();
        }
        c.category_seed_v1 = true;
    });
    println!("🎫 Set the open-ticket category for home guild ({home})");
}

/// The wasteland server's own tickets, matching the panel it already runs:
/// the same five options, all logging to one channel.
///
/// Not tied to GUILD_ID, the same way the faction applications aren't: these
/// belong to that server and nowhere else, so the id is the condition rather
/// than a home-guild check.
const WASTELAND_GUILD: &str = "1541171641218764850";
const WASTELAND_PANEL: &str = "1541755760059613215";
/// Every type logs to the same place here, rather than one channel each.
const WASTELAND_LOGS: &str = "1541755732867944510";

pub fn migrate_wasteland_tickets() {
    if get_ticket_config(WASTELAND_GUILD).wasteland_seed_v1 {
        return;
    }
    let types = [
        ("report_player", "Report Player", "🚨"),
        ("staff_report", "Staff Report", "🛡️"),
        ("faction_report", "Faction Report", "🪖"),
        ("general_support", "General Support", "🎫"),
        ("ban_appeal", "Ban Appeal", "⚖️"),
    ];
    update_ticket_config(WASTELAND_GUILD, |c| {
        c.wasteland_seed_v1 = true;
        c.panel_channel_id = WASTELAND_PANEL.to_string();
        // Left blank on purpose: with no category set, the first ticket opened
        // creates a "Tickets" category and everything lands in it. Point it
        // somewhere specific with `/tickets category` if you'd rather choose.
        c.types = types
            .into_iter()
            .map(|(key, label, emoji)| TicketType {
                key: key.to_string(),
                label: label.to_string(),
                emoji: emoji.to_string(),
                log_channel_id: WASTELAND_LOGS.to_string(),
                ..Default::default()
            })
            .collect();
    });
    println!("🎫 Seeded the ticket panel and five ticket types for {WASTELAND_GUILD}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket_type(key: &str, roles: &[&str]) -> TicketType {
        TicketType {
            key: key.into(),
            label: key.into(),
            support_role_ids: roles.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        }
    }

    /// A type with no roles of its own keeps behaving the way every ticket did
    /// before per-type roles existed: the server mod role handles it.
    #[test]
    fn a_type_without_roles_falls_back_to_the_mod_role() {
        assert_eq!(ticket_type("support", &[]).support_roles("42"), vec!["42".to_string()]);
    }

    /// And a type with its own roles does not also drag the mod role in, which
    /// is the whole point of pointing one kind of ticket at one team.
    #[test]
    fn a_type_with_roles_uses_only_its_own() {
        assert_eq!(ticket_type("reports", &["7", "8"]).support_roles("42"), vec!["7".to_string(), "8".to_string()]);
    }

    /// No roles anywhere is not an error, and must not produce an empty
    /// mention that would ping nothing and deny everyone.
    #[test]
    fn no_roles_configured_anywhere_is_empty() {
        assert!(ticket_type("support", &[]).support_roles("").is_empty());
    }
}
