//! Per-guild security thresholds.
//!
//! Every value has a bot-wide default read from the environment (see
//! `common::config`). A server overrides any of them with `/config`, and the
//! override lives in that server's own settings row, keyed by `Tunable::key`,
//! so one server tuning its anti-nuke can never move another's.

use std::collections::BTreeMap;

use crate::common::config::CONFIG;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Module {
    AntiNuke,
    AntiRaid,
    AntiSpam,
    Moderation,
}

impl Module {
    pub fn label(self) -> &'static str {
        match self {
            Module::AntiNuke => "Anti-Nuke",
            Module::AntiRaid => "Anti-Raid",
            Module::AntiSpam => "Anti-Spam",
            Module::Moderation => "Moderation",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tunable {
    NukeWindowSec,
    NukeChannelDelete,
    NukeChannelCreate,
    NukeRoleDelete,
    NukeRoleCreate,
    NukeBan,
    NukeKick,
    NukeWebhook,
    NukeEmoji,
    NukeTotal,
    NukePermEscalation,
    NukeKickAddedBots,

    RaidJoinThreshold,
    RaidWindowSec,
    RaidLockdownMin,
    RaidKickNewAccounts,
    RaidMinAccountAgeMin,

    SpamMessageThreshold,
    SpamWindowMs,
    SpamMuteMin,
    SpamMentionLimit,
    SpamDuplicateLimit,
    SpamBlockInvites,
    SpamBlockScams,
    SpamExemptStaff,

    ModBanLimit,
    ModKickLimit,
    ModMuteLimit,
    ModPurgeLimit,
    ModLockdownLimit,
    ModWarnLimit,
    ModWindowMin,
    WarnMuteAt,
    WarnKickAt,
    WarnBanAt,
    WarnMuteMin,
}

const BOOL: (i64, i64) = (0, 1);

impl Tunable {
    pub const ALL: [Tunable; 36] = [
        Tunable::NukeWindowSec,
        Tunable::NukeChannelDelete,
        Tunable::NukeChannelCreate,
        Tunable::NukeRoleDelete,
        Tunable::NukeRoleCreate,
        Tunable::NukeBan,
        Tunable::NukeKick,
        Tunable::NukeWebhook,
        Tunable::NukeEmoji,
        Tunable::NukeTotal,
        Tunable::NukePermEscalation,
        Tunable::NukeKickAddedBots,
        Tunable::RaidJoinThreshold,
        Tunable::RaidWindowSec,
        Tunable::RaidLockdownMin,
        Tunable::RaidKickNewAccounts,
        Tunable::RaidMinAccountAgeMin,
        Tunable::SpamMessageThreshold,
        Tunable::SpamWindowMs,
        Tunable::SpamMuteMin,
        Tunable::SpamMentionLimit,
        Tunable::SpamDuplicateLimit,
        Tunable::SpamBlockInvites,
        Tunable::SpamBlockScams,
        Tunable::SpamExemptStaff,
        Tunable::ModBanLimit,
        Tunable::ModKickLimit,
        Tunable::ModMuteLimit,
        Tunable::ModPurgeLimit,
        Tunable::ModLockdownLimit,
        Tunable::ModWarnLimit,
        Tunable::ModWindowMin,
        Tunable::WarnMuteAt,
        Tunable::WarnKickAt,
        Tunable::WarnBanAt,
        Tunable::WarnMuteMin,
    ];

    /// The stored key. Changing one orphans every override saved under it.
    pub fn key(self) -> &'static str {
        use Tunable::*;
        match self {
            NukeWindowSec => "nuke.window_sec",
            NukeChannelDelete => "nuke.channel_delete",
            NukeChannelCreate => "nuke.channel_create",
            NukeRoleDelete => "nuke.role_delete",
            NukeRoleCreate => "nuke.role_create",
            NukeBan => "nuke.ban",
            NukeKick => "nuke.kick",
            NukeWebhook => "nuke.webhook",
            NukeEmoji => "nuke.emoji",
            NukeTotal => "nuke.total",
            NukePermEscalation => "nuke.permission_escalation",
            NukeKickAddedBots => "nuke.kick_added_bots",
            RaidJoinThreshold => "raid.join_threshold",
            RaidWindowSec => "raid.window_sec",
            RaidLockdownMin => "raid.lockdown_min",
            RaidKickNewAccounts => "raid.kick_new_accounts",
            RaidMinAccountAgeMin => "raid.min_account_age_min",
            SpamMessageThreshold => "spam.message_threshold",
            SpamWindowMs => "spam.window_ms",
            SpamMuteMin => "spam.mute_min",
            SpamMentionLimit => "spam.mention_limit",
            SpamDuplicateLimit => "spam.duplicate_limit",
            SpamBlockInvites => "spam.block_invites",
            SpamBlockScams => "spam.block_scams",
            SpamExemptStaff => "spam.exempt_staff",
            ModBanLimit => "mod.ban_limit",
            ModKickLimit => "mod.kick_limit",
            ModMuteLimit => "mod.mute_limit",
            ModPurgeLimit => "mod.purge_limit",
            ModLockdownLimit => "mod.lockdown_limit",
            ModWarnLimit => "mod.warn_limit",
            ModWindowMin => "mod.window_min",
            WarnMuteAt => "warn.mute_at",
            WarnKickAt => "warn.kick_at",
            WarnBanAt => "warn.ban_at",
            WarnMuteMin => "warn.mute_min",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.key() == key)
    }

    pub fn module(self) -> Module {
        match self.key().split('.').next() {
            Some("nuke") => Module::AntiNuke,
            Some("raid") => Module::AntiRaid,
            Some("spam") => Module::AntiSpam,
            _ => Module::Moderation,
        }
    }

    pub fn label(self) -> &'static str {
        use Tunable::*;
        match self {
            NukeWindowSec => "Detection window (seconds)",
            NukeChannelDelete => "Channel deletes",
            NukeChannelCreate => "Channel creates",
            NukeRoleDelete => "Role deletes",
            NukeRoleCreate => "Role creates",
            NukeBan => "Bans",
            NukeKick => "Kicks / prunes",
            NukeWebhook => "Webhook creates",
            NukeEmoji => "Emoji / sticker deletes",
            NukeTotal => "Any destructive mix (0 = off)",
            NukePermEscalation => "Permission escalations",
            NukeKickAddedBots => "Kick bots added by non-whitelisted users",
            RaidJoinThreshold => "Joins that trigger a lockdown",
            RaidWindowSec => "Join window (seconds)",
            RaidLockdownMin => "Lockdown length (minutes)",
            RaidKickNewAccounts => "Turn away new accounts during a lockdown",
            RaidMinAccountAgeMin => "New account = younger than (minutes)",
            SpamMessageThreshold => "Messages that count as a flood",
            SpamWindowMs => "Flood window (milliseconds)",
            SpamMuteMin => "Spam mute length (minutes)",
            SpamMentionLimit => "Mentions in one message",
            SpamDuplicateLimit => "Repeated identical messages",
            SpamBlockInvites => "Block invite links",
            SpamBlockScams => "Block scam / grabber links",
            SpamExemptStaff => "Staff are exempt",
            ModBanLimit => "Bans per mod",
            ModKickLimit => "Kicks per mod",
            ModMuteLimit => "Mutes per mod",
            ModPurgeLimit => "Purges per mod",
            ModLockdownLimit => "Lockdowns per mod",
            ModWarnLimit => "Warns per mod",
            ModWindowMin => "Mod limit window (minutes)",
            WarnMuteAt => "Auto-mute at N warnings (0 = off)",
            WarnKickAt => "Auto-kick at N warnings (0 = off)",
            WarnBanAt => "Auto-ban at N warnings (0 = off)",
            WarnMuteMin => "Auto-mute length (minutes)",
        }
    }

    /// Inclusive bounds a stored value is held to.
    pub fn range(self) -> (i64, i64) {
        use Tunable::*;
        match self {
            NukeKickAddedBots | RaidKickNewAccounts | SpamBlockInvites | SpamBlockScams | SpamExemptStaff => BOOL,
            NukeWindowSec => (1, 300),
            NukeTotal => (0, 500),
            NukeChannelDelete | NukeChannelCreate | NukeRoleDelete | NukeRoleCreate | NukeBan | NukeKick
            | NukeWebhook | NukeEmoji | NukePermEscalation => (1, 500),
            RaidJoinThreshold => (2, 1000),
            RaidWindowSec => (1, 3600),
            RaidLockdownMin => (1, 10_080),
            RaidMinAccountAgeMin => (0, 525_600),
            SpamMessageThreshold => (2, 100),
            SpamWindowMs => (500, 60_000),
            SpamMuteMin | WarnMuteMin => (1, 40_320),
            SpamMentionLimit => (2, 100),
            SpamDuplicateLimit => (2, 50),
            ModBanLimit | ModKickLimit | ModMuteLimit | ModPurgeLimit | ModLockdownLimit | ModWarnLimit => (0, 10_000),
            ModWindowMin => (1, 10_080),
            WarnMuteAt | WarnKickAt | WarnBanAt => (0, 100),
        }
    }

    pub fn is_bool(self) -> bool {
        self.range() == BOOL
    }

    /// The bot-wide default, from the environment.
    pub fn env_default(self) -> i64 {
        use Tunable::*;
        let c = &*CONFIG;
        let v = match self {
            NukeWindowSec => c.nuke_window_ms / 1000,
            NukeChannelDelete => c.nuke_channel_threshold as i64,
            NukeChannelCreate => c.nuke_channel_create_thresh as i64,
            NukeRoleDelete => c.nuke_role_threshold as i64,
            NukeRoleCreate => c.nuke_role_create_thresh as i64,
            NukeBan => c.nuke_ban_threshold as i64,
            NukeKick => c.nuke_kick_threshold as i64,
            NukeWebhook => c.nuke_webhook_threshold as i64,
            NukeEmoji => c.nuke_emoji_threshold as i64,
            NukeTotal => c.nuke_total_threshold as i64,
            NukePermEscalation => 3,
            NukeKickAddedBots => (c.nuke_bot_add_action == "kick") as i64,
            RaidJoinThreshold => c.raid_join_threshold as i64,
            RaidWindowSec => c.raid_window_ms / 1000,
            RaidLockdownMin => c.raid_lockdown_min,
            RaidKickNewAccounts => c.raid_kick_new_on_lock as i64,
            RaidMinAccountAgeMin => c.raid_min_account_age_min,
            SpamMessageThreshold => c.spam_threshold as i64,
            SpamWindowMs => c.spam_window_ms,
            SpamMuteMin => c.spam_mute_min,
            SpamMentionLimit => c.spam_mention_limit as i64,
            SpamDuplicateLimit => c.spam_duplicate_limit as i64,
            SpamBlockInvites => c.spam_block_invites as i64,
            SpamBlockScams => c.scam_block as i64,
            SpamExemptStaff => c.spam_exempt_staff as i64,
            ModBanLimit => c.mod_ban_limit as i64,
            ModKickLimit => c.mod_kick_limit as i64,
            ModMuteLimit => c.mod_mute_limit as i64,
            ModPurgeLimit => c.mod_purge_limit as i64,
            ModLockdownLimit => c.mod_lockdown_limit as i64,
            ModWarnLimit => c.mod_warn_limit as i64,
            ModWindowMin => c.mod_window_ms / 60_000,
            WarnMuteAt => c.warn_mute_at as i64,
            WarnKickAt => c.warn_kick_at as i64,
            WarnBanAt => c.warn_ban_at as i64,
            WarnMuteMin => c.warn_mute_min,
        };
        let (lo, hi) = self.range();
        v.clamp(lo, hi)
    }

    pub fn format(self, v: i64) -> String {
        if self.is_bool() {
            if v != 0 { "on" } else { "off" }.to_string()
        } else {
            v.to_string()
        }
    }
}

/// A server's overrides, keyed by `Tunable::key`. Keys this build doesn't know
/// are kept as they are, so a downgrade doesn't throw away a newer setting.
pub type Overrides = BTreeMap<String, i64>;

/// The effective value: the server's override if it has a usable one, else
/// the environment default. A stored value outside the range (hand-edited, or
/// from a build with wider bounds) is clamped rather than trusted.
pub fn resolve(overrides: &Overrides, t: Tunable) -> i64 {
    let (lo, hi) = t.range();
    overrides.get(t.key()).map(|v| (*v).clamp(lo, hi)).unwrap_or_else(|| t.env_default())
}

pub struct NukeConfig {
    pub enabled: bool,
    pub window_ms: i64,
    pub channel_delete: usize,
    pub channel_create: usize,
    pub role_delete: usize,
    pub role_create: usize,
    pub ban: usize,
    pub kick: usize,
    pub webhook: usize,
    pub emoji: usize,
    /// 0 disables the aggregate counter.
    pub total: usize,
    pub perm_escalation: usize,
    pub kick_added_bots: bool,
}

pub struct RaidConfig {
    pub enabled: bool,
    pub join_threshold: usize,
    pub window_ms: i64,
    pub lockdown_min: i64,
    pub kick_new_accounts: bool,
    pub min_account_age_min: i64,
}

pub struct SpamConfig {
    pub enabled: bool,
    pub message_threshold: usize,
    pub window_ms: i64,
    pub mute_min: i64,
    pub mention_limit: usize,
    pub duplicate_limit: usize,
    pub block_invites: bool,
    pub block_scams: bool,
    pub exempt_staff: bool,
}

pub struct ModConfig {
    pub ban_limit: usize,
    pub kick_limit: usize,
    pub mute_limit: usize,
    pub purge_limit: usize,
    pub lockdown_limit: usize,
    pub warn_limit: usize,
    pub window_ms: i64,
    pub warn_mute_at: usize,
    pub warn_kick_at: usize,
    pub warn_ban_at: usize,
    pub warn_mute_min: i64,
}

impl ModConfig {
    pub fn limit_for(&self, action: &str) -> usize {
        match action {
            "ban" => self.ban_limit,
            "kick" => self.kick_limit,
            "mute" => self.mute_limit,
            "purge" => self.purge_limit,
            "lockdown" => self.lockdown_limit,
            "warn" => self.warn_limit,
            _ => 0,
        }
    }

    pub fn window_hours(&self) -> i64 {
        (self.window_ms / 3_600_000).max(1)
    }
}

fn n(o: &Overrides, t: Tunable) -> usize {
    resolve(o, t).max(0) as usize
}
fn b(o: &Overrides, t: Tunable) -> bool {
    resolve(o, t) != 0
}

pub fn nuke(o: &Overrides, enabled: bool) -> NukeConfig {
    use Tunable::*;
    NukeConfig {
        enabled,
        window_ms: resolve(o, NukeWindowSec) * 1000,
        channel_delete: n(o, NukeChannelDelete),
        channel_create: n(o, NukeChannelCreate),
        role_delete: n(o, NukeRoleDelete),
        role_create: n(o, NukeRoleCreate),
        ban: n(o, NukeBan),
        kick: n(o, NukeKick),
        webhook: n(o, NukeWebhook),
        emoji: n(o, NukeEmoji),
        total: n(o, NukeTotal),
        perm_escalation: n(o, NukePermEscalation),
        kick_added_bots: b(o, NukeKickAddedBots),
    }
}

pub fn raid(o: &Overrides, enabled: bool) -> RaidConfig {
    use Tunable::*;
    RaidConfig {
        enabled,
        join_threshold: n(o, RaidJoinThreshold),
        window_ms: resolve(o, RaidWindowSec) * 1000,
        lockdown_min: resolve(o, RaidLockdownMin),
        kick_new_accounts: b(o, RaidKickNewAccounts),
        min_account_age_min: resolve(o, RaidMinAccountAgeMin),
    }
}

pub fn spam(o: &Overrides, enabled: bool) -> SpamConfig {
    use Tunable::*;
    SpamConfig {
        enabled,
        message_threshold: n(o, SpamMessageThreshold),
        window_ms: resolve(o, SpamWindowMs),
        mute_min: resolve(o, SpamMuteMin),
        mention_limit: n(o, SpamMentionLimit),
        duplicate_limit: n(o, SpamDuplicateLimit),
        block_invites: b(o, SpamBlockInvites),
        block_scams: b(o, SpamBlockScams),
        exempt_staff: b(o, SpamExemptStaff),
    }
}

pub fn moderation(o: &Overrides) -> ModConfig {
    use Tunable::*;
    ModConfig {
        ban_limit: n(o, ModBanLimit),
        kick_limit: n(o, ModKickLimit),
        mute_limit: n(o, ModMuteLimit),
        purge_limit: n(o, ModPurgeLimit),
        lockdown_limit: n(o, ModLockdownLimit),
        warn_limit: n(o, ModWarnLimit),
        window_ms: resolve(o, ModWindowMin) * 60_000,
        warn_mute_at: n(o, WarnMuteAt),
        warn_kick_at: n(o, WarnKickAt),
        warn_ban_at: n(o, WarnBanAt),
        warn_mute_min: resolve(o, WarnMuteMin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_round_trip() {
        let mut keys: Vec<_> = Tunable::ALL.iter().map(|t| t.key()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), Tunable::ALL.len());
        for t in Tunable::ALL {
            assert_eq!(Tunable::from_key(t.key()), Some(t));
        }
    }

    /// Discord allows at most 25 choices on one option, and `/config` offers
    /// one module's settings per subcommand.
    #[test]
    fn every_module_fits_in_one_choice_list() {
        for m in [Module::AntiNuke, Module::AntiRaid, Module::AntiSpam, Module::Moderation] {
            let count = Tunable::ALL.iter().filter(|t| t.module() == m).count();
            assert!(count > 0 && count <= 25, "{m:?} has {count} settings");
        }
    }

    #[test]
    fn env_defaults_sit_inside_their_ranges() {
        for t in Tunable::ALL {
            let (lo, hi) = t.range();
            let v = t.env_default();
            assert!(v >= lo && v <= hi, "{}: {v} not in {lo}..={hi}", t.key());
        }
    }

    #[test]
    fn an_override_wins_and_is_clamped() {
        let mut o = Overrides::new();
        assert_eq!(resolve(&o, Tunable::RaidJoinThreshold), Tunable::RaidJoinThreshold.env_default());
        o.insert("raid.join_threshold".into(), 25);
        assert_eq!(raid(&o, true).join_threshold, 25);
        o.insert("raid.join_threshold".into(), 1_000_000);
        assert_eq!(raid(&o, true).join_threshold, 1000);
        o.insert("raid.join_threshold".into(), -5);
        assert_eq!(raid(&o, true).join_threshold, 2);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut o = Overrides::new();
        o.insert("something.from_the_future".into(), 7);
        assert_eq!(nuke(&o, true).ban, Tunable::NukeBan.env_default() as usize);
    }
}
