//! Anti-Nuke engine.
//!
//! Scoped per guild - a user's actions in one server never count toward
//! thresholds in another. Fires once per real audit-log entry with a reliable
//! executor; bot-executed actions (i.e. our own commands) are skipped so
//! command paths remain the single counter for command-driven floods.
//!
//! All counter state lives behind one mutex that is never held across an
//! `.await`. Counting, the trip decision and claiming the response happen in
//! one critical section, so a burst of parallel events produces exactly one
//! response per attacker.

use once_cell::sync::Lazy;
use serenity::client::Context;
use serenity::http::HttpError;
use serenity::model::guild::audit_log::{
    Action, AuditLogEntry, Change, ChannelAction, EmojiAction, MemberAction, RoleAction,
    StickerAction, WebhookAction,
};
use serenity::model::guild::Member;
use serenity::model::id::{GuildId, RoleId, UserId};
use serenity::model::Permissions;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use crate::common::config::{now_ms, DANGER_PERMS_MASK};
use crate::common::embeds::{alert_owner, colors, sec_log};
use crate::common::guildinfo::GuildInfo;
use crate::common::permissions::{is_owner, is_whitelisted};
use crate::state::guild_settings;
use crate::state::tunables::{NukeConfig, Tunable};

/// Shared counter fed by EVERY destructive action, on top of the per-category
/// one. Without it a nuke that spreads itself across categories (a couple of
/// channel deletes, a couple of bans, a couple of webhooks) stays under every
/// individual threshold and never trips anything. This caps the total
/// regardless of the mix.
const TOTAL_KEY: &str = "allDestructive";

/// Audit-log entry ids remembered per guild, to drop redeliveries.
const SEEN_PER_GUILD: usize = 512;

#[derive(Default)]
struct UserState {
    /// Category -> event times (Discord time, ms), kept sorted.
    counts: HashMap<&'static str, Vec<i64>>,
    /// Newest event time seen for this user; windows are measured back from it.
    newest: i64,
    /// Local clock, only used to decide when the entry can be swept.
    touched: i64,
    /// A response is running; further events are ignored until it ends.
    responding: bool,
    /// Newest event time seen when the last response ended. Anything at or
    /// before it happened before that response and has been answered.
    handled_until: i64,
}

#[derive(Default)]
struct GuildState {
    users: HashMap<UserId, UserState>,
    seen: HashSet<u64>,
    seen_order: VecDeque<u64>,
    touched: i64,
}

static TRACKER: Lazy<Mutex<HashMap<GuildId, GuildState>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn tracker() -> std::sync::MutexGuard<'static, HashMap<GuildId, GuildState>> {
    TRACKER.lock().unwrap_or_else(|e| e.into_inner())
}

/// When a snowflake (audit-log entry, interaction) was created, in ms.
///
/// Counting on Discord's timestamps rather than arrival time keeps a backlog
/// replayed after a reconnect from looking like a burst, and keeps events
/// from the gateway and from commands on one clock.
pub fn event_time(id: u64) -> i64 {
    (id >> 22) as i64 + 1_420_070_400_000
}

/// True the first time an audit-log entry id is seen in a guild. Discord can
/// deliver the same entry twice; counting it twice would halve a threshold.
fn first_sighting(guild_id: GuildId, entry_id: u64) -> bool {
    let mut map = tracker();
    let g = map.entry(guild_id).or_default();
    g.touched = now_ms();
    if !g.seen.insert(entry_id) {
        return false;
    }
    g.seen_order.push_back(entry_id);
    if g.seen_order.len() > SEEN_PER_GUILD {
        if let Some(old) = g.seen_order.pop_front() {
            g.seen.remove(&old);
        }
    }
    true
}

/// Which counter actually crossed its line, so the alert can name the real reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trip {
    Category,
    Total,
}

/// Ownership of the response for one (guild, user). Releases it on drop,
/// including when the response panics or its future is dropped, so the user
/// can never be left permanently marked as "being handled".
pub struct ResponseGuard {
    guild_id: GuildId,
    user_id: UserId,
}

impl Drop for ResponseGuard {
    fn drop(&mut self) {
        let mut map = tracker();
        if let Some(u) = map
            .get_mut(&self.guild_id)
            .and_then(|g| g.users.get_mut(&self.user_id))
        {
            u.responding = false;
            u.counts.clear();
            u.handled_until = u.newest;
            u.touched = now_ms();
        }
    }
}

/// A threshold crossing. Holding it means holding the response for that user.
pub struct Tripped {
    pub trip: Trip,
    guard: ResponseGuard,
}

/// Prune one counter to the window ending at `newest`, add `at`, and return
/// the count.
fn record(arr: &mut Vec<i64>, at: i64, newest: i64, window_ms: i64) -> usize {
    arr.retain(|t| newest - *t < window_ms);
    let pos = arr.partition_point(|t| *t <= at);
    arr.insert(pos, at);
    arr.len()
}

/// Count one destructive action against its category and the shared counter.
///
/// Counting, the trip decision, the reset and claiming the response are one
/// critical section. A trip clears every counter for the user and marks them
/// as being handled, so the rest of a parallel burst is ignored rather than
/// tripping again. Returns `None` when nothing tripped, when a response for
/// this user is already running, or when the event is older than the window.
pub fn bump_destructive(
    guild_id: GuildId,
    user_id: UserId,
    key: &'static str,
    threshold: usize,
    cfg: &NukeConfig,
    at: i64,
) -> Option<Tripped> {
    let now = now_ms();
    let mut map = tracker();
    let g = map.entry(guild_id).or_default();
    g.touched = now;
    let u = g.users.entry(user_id).or_default();
    u.touched = now;
    u.newest = u.newest.max(at);
    // Entries still streaming in for actions the response already answered
    // would otherwise refill the counter and trip a second, duplicate one.
    if u.responding || at <= u.handled_until {
        return None;
    }
    let newest = u.newest;
    // Arrived after the window it belonged to had already passed.
    if newest - at >= cfg.window_ms {
        return None;
    }

    let in_category = record(u.counts.entry(key).or_default(), at, newest, cfg.window_ms);
    // A total threshold of 0 disables the aggregate check entirely.
    let in_total = if cfg.total > 0 {
        record(
            u.counts.entry(TOTAL_KEY).or_default(),
            at,
            newest,
            cfg.window_ms,
        )
    } else {
        0
    };

    let trip = if in_category >= threshold {
        Trip::Category
    } else if cfg.total > 0 && in_total >= cfg.total {
        Trip::Total
    } else {
        return None;
    };
    u.counts.clear();
    u.responding = true;
    Some(Tripped {
        trip,
        guard: ResponseGuard { guild_id, user_id },
    })
}

fn responding(guild_id: GuildId, user_id: UserId) -> bool {
    tracker()
        .get(&guild_id)
        .and_then(|g| g.users.get(&user_id))
        .is_some_and(|u| u.responding)
}

pub fn total_reason(cfg: &NukeConfig) -> String {
    format!(
        "{}+ destructive actions in {}s",
        cfg.total,
        cfg.window_ms / 1000
    )
}

/// Drop users who haven't acted within the longest possible window, and
/// guilds with nobody left in them. Never drops a user mid-response.
pub fn sweep() {
    let now = now_ms();
    let idle = Tunable::NukeWindowSec.range().1 * 1000 + 60_000;
    tracker().retain(|_, g| {
        g.users
            .retain(|_, u| u.responding || now - u.touched < idle);
        !g.users.is_empty() || now - g.touched < idle
    });
}

/// Forget a guild's counters (the bot left it). A response still running
/// there releases cleanly: its guard finds nothing to reset.
pub fn forget_guild(guild_id: GuildId) {
    tracker().remove(&guild_id);
}

// ── Discord errors ────────────────────────────────────────────

fn http_status(e: &serenity::Error) -> Option<(u16, isize)> {
    match e {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(r)) => {
            Some((r.status_code.as_u16(), r.error.code))
        }
        _ => None,
    }
}

/// Server errors and dropped connections are worth one retry; a 4xx
/// (missing permissions, hierarchy) will fail the same way again. 429s are
/// already waited out and retried inside serenity's rate limiter.
fn transient_status(status: u16) -> bool {
    status >= 500
}

fn is_transient(e: &serenity::Error) -> bool {
    match e {
        serenity::Error::Http(HttpError::Request(_)) => true,
        _ => http_status(e).is_some_and(|(s, _)| transient_status(s)),
    }
}

/// Unknown Member / Unknown User: the person isn't in the guild.
fn gone_status(status: u16, code: isize) -> bool {
    matches!(code, 10007 | 10013) || status == 404
}

async fn retry_once<T, F, Fut>(mut call: F) -> serenity::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = serenity::Result<T>>,
{
    match call().await {
        Err(e) if is_transient(&e) => {
            tokio::time::sleep(Duration::from_millis(500)).await;
            call().await
        }
        other => other,
    }
}

enum Presence {
    Member(Box<Member>),
    /// Not in the guild, so they hold no roles and can't be role-whitelisted.
    Absent,
    /// Couldn't tell.
    Unknown(String),
}

async fn lookup_member(ctx: &Context, guild_id: GuildId, user_id: UserId) -> Presence {
    if let Some(m) = ctx
        .cache
        .guild(guild_id)
        .and_then(|g| g.members.get(&user_id).cloned())
    {
        return Presence::Member(Box::new(m));
    }
    match retry_once(|| guild_id.member(&ctx.http, user_id)).await {
        Ok(m) => Presence::Member(Box::new(m)),
        Err(e) if http_status(&e).is_some_and(|(s, c)| gone_status(s, c)) => Presence::Absent,
        Err(e) => Presence::Unknown(e.to_string()),
    }
}

// ── Whitelist ─────────────────────────────────────────────────

/// Whether the whitelist can be settled from ids alone.
///
/// Three of the four whitelist conditions are id-only: bot owner, guild owner,
/// and the whitelisted-user list. Only the fourth, the whitelisted-role list,
/// needs the member object. When a guild whitelists no roles the fourth cannot
/// apply, so the ban goes out without waiting on a member fetch first.
#[derive(PartialEq, Eq, Debug)]
pub enum WhitelistCheck {
    /// Settled without the member: true means immune.
    Decided(bool),
    /// The guild whitelists roles, so the member's roles have to be read.
    NeedsMember,
}

pub fn whitelist_from_ids(
    guild_id: GuildId,
    user_id: UserId,
    guild_owner_id: UserId,
) -> WhitelistCheck {
    if is_owner(user_id) || user_id == guild_owner_id {
        return WhitelistCheck::Decided(true);
    }
    let uid = user_id.to_string();
    guild_settings::with(&guild_id.to_string(), |g| {
        if g.nuke_whitelist_user_ids.contains(&uid) {
            WhitelistCheck::Decided(true)
        } else if g.nuke_whitelist_role_ids.is_empty() {
            WhitelistCheck::Decided(false)
        } else {
            WhitelistCheck::NeedsMember
        }
    })
}

/// Is this person exempt from anti-nuke, for the non-punitive paths (rolling
/// back a permission change, the settings alert)? A member record that can't
/// be read counts as not exempt there, since the rollback is reversible.
async fn is_immune(ctx: &Context, guild_id: GuildId, user_id: UserId, owner_id: UserId) -> bool {
    match whitelist_from_ids(guild_id, user_id, owner_id) {
        WhitelistCheck::Decided(v) => v,
        WhitelistCheck::NeedsMember => match lookup_member(ctx, guild_id, user_id).await {
            Presence::Member(m) => is_whitelisted(&m, owner_id),
            Presence::Absent | Presence::Unknown(_) => false,
        },
    }
}

// ── Response ──────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Kick {
    /// The ban went through; nothing else was needed.
    NotNeeded,
    Kicked,
    Failed(String),
    NotInServer,
}

#[derive(Debug)]
struct Outcome {
    /// `None` when the ban went through.
    ban_error: Option<String>,
    stripped: Vec<RoleId>,
    strip_error: Option<String>,
    kick: Kick,
}

impl Outcome {
    /// They can't keep going: banned, kicked, or not in the server.
    fn contained(&self) -> bool {
        self.ban_error.is_none() || matches!(self.kick, Kick::Kicked | Kick::NotInServer)
    }

    fn what_i_did(&self) -> String {
        let Some(ban_error) = &self.ban_error else {
            return "banned them straight away.".to_string();
        };
        let mut parts = vec![format!("couldn't ban them ({ban_error}).")];
        if let Some(e) = &self.strip_error {
            parts.push(format!("Couldn't pull their dangerous roles: {e}."));
        } else if !self.stripped.is_empty() {
            let list = self
                .stripped
                .iter()
                .map(|r| format!("<@&{r}>"))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("Pulled their dangerous roles: {list}."));
        }
        parts.push(match &self.kick {
            Kick::Kicked => "Kicked them instead.".to_string(),
            Kick::NotInServer => "They're no longer in the server.".to_string(),
            Kick::Failed(e) => {
                format!("The kick failed too ({e}). **Please check my role position right away.**")
            }
            Kick::NotNeeded => String::new(),
        });
        parts.retain(|p| !p.is_empty());
        parts.join(" ")
    }
}

/// Ban the executor immediately; if that is refused, fall back to stripping
/// their dangerous roles and kicking. One alert either way.
///
/// Taking `Tripped` means only the caller that won the trip can respond, and
/// the response slot is released when this returns, however it returns.
pub async fn nuke_response(
    ctx: &Context,
    guild_id: GuildId,
    user_id: UserId,
    reason: &str,
    tripped: Tripped,
) {
    debug_assert!(tripped.guard.guild_id == guild_id && tripped.guard.user_id == user_id);
    run_nuke_response(ctx, guild_id, user_id, reason).await;
    drop(tripped);
}

async fn run_nuke_response(ctx: &Context, guild_id: GuildId, user_id: UserId, reason: &str) {
    let Some(owner_id) = ctx.cache.guild(guild_id).map(|g| g.owner_id) else {
        eprintln!("⚠️ [{guild_id}] anti-nuke tripped on {user_id} but the guild isn't cached; can't check the owner, so no action taken");
        return;
    };

    // Re-guard: never punish an owner or a whitelisted user.
    let mut presence = None;
    match whitelist_from_ids(guild_id, user_id, owner_id) {
        WhitelistCheck::Decided(true) => return,
        WhitelistCheck::Decided(false) => {}
        WhitelistCheck::NeedsMember => match lookup_member(ctx, guild_id, user_id).await {
            Presence::Member(m) if is_whitelisted(&m, owner_id) => return,
            Presence::Unknown(e) => {
                // The role whitelist can't be checked. Punishing might hit a
                // whitelisted admin; say so loudly instead of acting blind.
                alert_owner(
                    ctx,
                    guild_id,
                    &format!(
                        "Anti-nuke tripped on <@{user_id}> (`{user_id}`) - {reason} - but I couldn't read their roles to check the whitelist ({e}), so I haven't acted. **Check the audit log now.**"
                    ),
                    colors::DANGER,
                    "Anti-Nuke Needs a Look",
                )
                .await;
                return;
            }
            p => presence = Some(p),
        },
    }

    // The ban is the first request made, and nothing waits in front of it.
    // Stripping roles is only a fallback for when the ban is refused.
    let audit_reason: String = format!("Anti-Nuke: {reason}").chars().take(500).collect();
    let ban = retry_once(|| guild_id.ban_with_reason(&ctx.http, user_id, 0, &audit_reason)).await;

    let outcome = match ban {
        Ok(()) => Outcome {
            ban_error: None,
            stripped: Vec::new(),
            strip_error: None,
            kick: Kick::NotNeeded,
        },
        Err(ban_error) => {
            let presence = match presence {
                Some(p) => p,
                None => lookup_member(ctx, guild_id, user_id).await,
            };
            fallback(
                ctx,
                guild_id,
                user_id,
                &audit_reason,
                ban_error.to_string(),
                presence,
            )
            .await
        }
    };

    let contained = outcome.contained();
    alert_owner(
        ctx,
        guild_id,
        &format!(
            "Anti-nuke just kicked in on <@{user_id}> (`{user_id}`).\n**What set it off:** {reason}\n**What I did:** {}",
            outcome.what_i_did()
        ),
        if contained { colors::NUKE } else { colors::DANGER },
        if contained { "Anti-Nuke Triggered" } else { "Anti-Nuke Needs a Look" },
    )
    .await;
    let log = match outcome.ban_error {
        None => format!("Banned <@{user_id}> - {reason}"),
        Some(_) => format!("<@{user_id}> - {reason}: {}", outcome.what_i_did()),
    };
    sec_log(
        ctx,
        guild_id,
        "Anti-Nuke",
        &log,
        if contained {
            colors::NUKE
        } else {
            colors::WARN
        },
    )
    .await;
}

/// The ban was refused (usually a role above the bot): de-perm and kick.
async fn fallback(
    ctx: &Context,
    guild_id: GuildId,
    user_id: UserId,
    audit_reason: &str,
    ban_error: String,
    presence: Presence,
) -> Outcome {
    let mut outcome = Outcome {
        ban_error: Some(ban_error),
        stripped: Vec::new(),
        strip_error: None,
        kick: Kick::NotInServer,
    };
    if matches!(presence, Presence::Absent) {
        return outcome;
    }
    if let Presence::Member(member) = &presence {
        let to_remove = GuildInfo::from_cache(ctx, guild_id)
            .map(|i| i.dangerous_editable_roles(&member.roles))
            .unwrap_or_default();
        // One request per role; stop at the first refusal but keep what landed.
        for role in to_remove {
            match retry_once(|| {
                ctx.http
                    .remove_member_role(guild_id, user_id, role, Some(audit_reason))
            })
            .await
            {
                Ok(()) => outcome.stripped.push(role),
                Err(e) => {
                    outcome.strip_error = Some(e.to_string());
                    break;
                }
            }
        }
    }
    outcome.kick =
        match retry_once(|| guild_id.kick_with_reason(&ctx.http, user_id, audit_reason)).await {
            Ok(()) => Kick::Kicked,
            Err(e) if http_status(&e).is_some_and(|(s, c)| gone_status(s, c)) => Kick::NotInServer,
            Err(e) => Kick::Failed(e.to_string()),
        };
    outcome
}

// ── Audit log ─────────────────────────────────────────────────

/// The executor worth judging: a real user that isn't this bot.
fn judged_executor(executor: UserId, me: UserId) -> Option<UserId> {
    (executor.get() != 0 && executor != me).then_some(executor)
}

/// A single-category detector: its counter and threshold, plus the words
/// for the alert, which is only formatted if it trips.
struct Detector {
    key: &'static str,
    threshold: usize,
    verb: &'static str,
    what: &'static str,
}

impl Detector {
    fn reason(&self, cfg: &NukeConfig) -> String {
        format!(
            "{} {}+ {} in {}s",
            self.verb,
            self.threshold,
            self.what,
            cfg.window_ms / 1000
        )
    }
}

fn simple_detector(action: &Action, cfg: &NukeConfig) -> Option<Detector> {
    let (key, threshold, verb, what) = match action {
        Action::Channel(ChannelAction::Delete) => {
            ("chDel", cfg.channel_delete, "Deleted", "channels")
        }
        Action::Channel(ChannelAction::Create) => {
            ("chCreate", cfg.channel_create, "Created", "channels")
        }
        Action::Role(RoleAction::Delete) => ("roleDel", cfg.role_delete, "Deleted", "roles"),
        Action::Role(RoleAction::Create) => ("roleCreate", cfg.role_create, "Created", "roles"),
        Action::Member(MemberAction::BanAdd) => ("bans", cfg.ban, "Issued", "bans"),
        Action::Member(MemberAction::Kick) | Action::Member(MemberAction::Prune) => {
            ("kicks", cfg.kick, "Removed", "members")
        }
        Action::Emoji(EmojiAction::Delete) | Action::Sticker(StickerAction::Delete) => {
            ("emojiDel", cfg.emoji, "Deleted", "emojis/stickers")
        }
        Action::Webhook(WebhookAction::Create) => ("webhooks", cfg.webhook, "Created", "webhooks"),
        _ => return None,
    };
    Some(Detector {
        key,
        threshold,
        verb,
        what,
    })
}

/// A role change (or a new role) that grants a dangerous permission, as
/// (old, new) permissions.
fn escalation(action: &Action, changes: Option<&[Change]>) -> Option<(Permissions, Permissions)> {
    if !matches!(
        action,
        Action::Role(RoleAction::Update | RoleAction::Create)
    ) {
        return None;
    }
    let (old_p, new_p) = permission_change(changes?)?;
    let gained = new_p & !old_p;
    gained
        .intersects(*DANGER_PERMS_MASK)
        .then_some((old_p, new_p))
}

/// One audit-log entry, dispatched to the matching detector.
pub async fn on_audit_log_entry(ctx: &Context, entry: &AuditLogEntry, guild_id: GuildId) {
    let Some(executor_id) = judged_executor(entry.user_id, ctx.cache.current_user().id) else {
        return;
    };
    let cfg = guild_settings::nuke(&guild_id.to_string());
    if !cfg.enabled {
        return;
    }
    if !first_sighting(guild_id, entry.id.get()) {
        return;
    }
    let at = event_time(entry.id.get());

    // Owner, bot owner and whitelisted-user ids are exempt outright, before
    // anything is counted. Role-whitelisted users are counted (settling that
    // needs their roles, and counting must not wait on a fetch) and are let
    // off when the trip is evaluated.
    let owner_id = ctx.cache.guild(guild_id).map(|g| g.owner_id);
    if let Some(owner) = owner_id {
        if whitelist_from_ids(guild_id, executor_id, owner) == WhitelistCheck::Decided(true) {
            return;
        }
    } else if is_owner(executor_id) {
        return;
    }

    // A new role created with dangerous permissions is an escalation, and is
    // counted as one rather than as a role create too.
    if let Some((old_p, new_p)) = escalation(&entry.action, entry.changes.as_deref()) {
        return on_escalation(ctx, entry, guild_id, executor_id, &cfg, at, old_p, new_p).await;
    }

    if let Some(d) = simple_detector(&entry.action, &cfg) {
        if let Some(tripped) = bump_destructive(guild_id, executor_id, d.key, d.threshold, &cfg, at)
        {
            let reason = if tripped.trip == Trip::Category {
                d.reason(&cfg)
            } else {
                total_reason(&cfg)
            };
            nuke_response(ctx, guild_id, executor_id, &reason, tripped).await;
            if d.key == "webhooks" {
                remove_webhooks_by(ctx, guild_id, executor_id).await;
            }
        }
        return;
    }

    match entry.action {
        Action::Member(MemberAction::BotAdd) => {
            on_bot_add(ctx, entry, guild_id, executor_id, &cfg).await
        }
        Action::GuildUpdate => {
            let Some(owner) = owner_id else { return };
            if is_immune(ctx, guild_id, executor_id, owner).await {
                return;
            }
            alert_owner(
                ctx,
                guild_id,
                &format!("<@{executor_id}> changed the server settings. Might be worth a glance at the audit log."),
                colors::WARN,
                "Server Settings Changed",
            )
            .await;
        }
        _ => {}
    }
}

/// Clean up whatever webhooks this user made, best effort. One request for
/// the whole guild, after the ban rather than in front of it.
async fn remove_webhooks_by(ctx: &Context, guild_id: GuildId, user_id: UserId) {
    match guild_id.webhooks(&ctx.http).await {
        Ok(hooks) => {
            for wh in hooks
                .iter()
                .filter(|w| w.user.as_ref().map(|u| u.id) == Some(user_id))
            {
                if let Err(e) = wh.delete(&ctx.http).await {
                    eprintln!(
                        "⚠️ [{guild_id}] couldn't delete webhook {} made by {user_id}: {e}",
                        wh.id
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("⚠️ [{guild_id}] couldn't list webhooks to clean up after {user_id}: {e}")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn on_escalation(
    ctx: &Context,
    entry: &AuditLogEntry,
    guild_id: GuildId,
    executor_id: UserId,
    cfg: &NukeConfig,
    at: i64,
    old_p: Permissions,
    new_p: Permissions,
) {
    // Counted before any await: the immunity check, the rollback and the
    // alert are all round trips.
    let already_handling = responding(guild_id, executor_id);
    let tripped = bump_destructive(
        guild_id,
        executor_id,
        "permEsc",
        cfg.perm_escalation,
        cfg,
        at,
    );

    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else {
        return;
    };
    // Whitelisted people are allowed to hand out permissions: no rollback, no
    // alert, no response (dropping `tripped` releases it).
    if is_immune(ctx, guild_id, executor_id, info.owner_id).await {
        return;
    }

    let target_id = entry.target_id.map(|t| t.get()).unwrap_or(0);
    let role = RoleId::new(target_id.max(1));
    // A role created moments ago may not be cached yet; it's at the bottom,
    // so let Discord decide rather than skip the rollback.
    let editable = info
        .roles
        .get(&role)
        .is_none_or(|_| info.role_editable(role));
    // For a new role, "old" is empty; keep what it was given minus the
    // dangerous part instead of wiping it.
    let revert_to = if matches!(entry.action, Action::Role(RoleAction::Create)) {
        new_p - *DANGER_PERMS_MASK
    } else {
        old_p
    };
    let reverted = if should_revert_escalation(target_id, editable) {
        retry_once(|| {
            guild_id.edit_role(
                &ctx.http,
                role,
                serenity::builder::EditRole::new()
                    .permissions(revert_to)
                    .audit_log_reason("Anti-Nuke: dangerous permission change"),
            )
        })
        .await
        .map(|_| ())
    } else {
        Err(serenity::Error::Other("role not editable"))
    };

    if let Some(tripped) = tripped {
        let reason = if tripped.trip == Trip::Category {
            "Repeated permission escalation".to_string()
        } else {
            total_reason(cfg)
        };
        nuke_response(ctx, guild_id, executor_id, &reason, tripped).await;
        return;
    }
    // Part of an attack already being answered; that alert covers it.
    if already_handling {
        return;
    }
    let what = match reverted {
        Ok(()) => "I've rolled that back.".to_string(),
        Err(e) => format!("I couldn't roll it back ({e}), so please check that role."),
    };
    alert_owner(
        ctx,
        guild_id,
        &format!("<@{executor_id}> just handed <@&{target_id}> some dangerous permissions. {what}"),
        colors::WARN,
        "Permission Change Reverted",
    )
    .await;
}

async fn on_bot_add(
    ctx: &Context,
    entry: &AuditLogEntry,
    guild_id: GuildId,
    executor_id: UserId,
    cfg: &NukeConfig,
) {
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else {
        return;
    };
    let executor = match whitelist_from_ids(guild_id, executor_id, info.owner_id) {
        WhitelistCheck::Decided(true) => return,
        _ => match lookup_member(ctx, guild_id, executor_id).await {
            Presence::Member(m) if is_whitelisted(&m, info.owner_id) => return,
            p => p,
        },
    };

    let target_id = entry.target_id.map(|t| t.get()).unwrap_or(0);
    let kick_note = if !cfg.kick_added_bots {
        "you'll want to review this.".to_string()
    } else if target_id == 0 {
        "I couldn't tell which bot it was, so it's still here.".to_string()
    } else {
        match retry_once(|| {
            guild_id.kick_with_reason(
                &ctx.http,
                UserId::new(target_id),
                "Anti-nuke: unauthorized bot add",
            )
        })
        .await
        {
            Ok(()) => "I've kicked it back out.".to_string(),
            Err(e) => format!("I couldn't kick it ({e}), so it's still here."),
        }
    };

    // Strip EVERY removable role from whoever added the bot.
    // (Skips @everyone, managed/integration roles, and anything above
    // my top role.)
    let roles_note = match &executor {
        Presence::Member(m) => {
            let (removable, unstrippable): (Vec<RoleId>, Vec<RoleId>) = m
                .roles
                .iter()
                .copied()
                .filter(|r| r.get() != guild_id.get())
                .partition(|r| info.role_editable(*r));
            let mut stripped = Vec::new();
            let mut error = None;
            for role in &removable {
                match retry_once(|| {
                    ctx.http.remove_member_role(
                        guild_id,
                        executor_id,
                        *role,
                        Some("Anti-nuke: added a bot"),
                    )
                })
                .await
                {
                    Ok(()) => stripped.push(*role),
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            let mentions = |rs: &[RoleId]| {
                rs.iter()
                    .map(|r| format!("<@&{r}>"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut note = format!(
                "I also pulled **{}** role{} off <@{executor_id}>: {}",
                stripped.len(),
                if stripped.len() == 1 { "" } else { "s" },
                if stripped.is_empty() {
                    "none".to_string()
                } else {
                    mentions(&stripped)
                }
            );
            if let Some(e) = error {
                note.push_str(&format!("\nThe rest failed: {e}"));
            }
            if !unstrippable.is_empty() {
                note.push_str(&format!(
                    "\nCouldn't take these (managed or above me): {}",
                    mentions(&unstrippable)
                ));
            }
            note
        }
        Presence::Absent => format!(
            "<@{executor_id}> isn't in the server any more, so there were no roles to pull."
        ),
        Presence::Unknown(e) => {
            format!("I couldn't read <@{executor_id}>'s roles ({e}), so I haven't pulled any.")
        }
    };

    alert_owner(
        ctx,
        guild_id,
        &format!("<@{executor_id}> added the bot <@{target_id}> - {kick_note}\n{roles_note}"),
        colors::DANGER,
        "Bot Added",
    )
    .await;
}

/// Whether an escalated permission change is one we can and should roll back.
///
/// Immunity is checked before this is reached, since a whitelisted person's
/// change is not rolled back at all rather than rolled back quietly.
fn should_revert_escalation(target_id: u64, role_editable: bool) -> bool {
    target_id != 0 && role_editable
}

/// Pull the old/new permission bitfields out of a role's change list.
fn permission_change(changes: &[Change]) -> Option<(Permissions, Permissions)> {
    changes.iter().find_map(|c| match c {
        Change::Permissions { old, new } => Some((
            old.unwrap_or_else(Permissions::empty),
            new.unwrap_or_else(Permissions::empty),
        )),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Counters are global and tests run in parallel, so every test works
    /// against ids nobody else touches rather than clearing the map.
    fn fresh() -> u64 {
        static N: AtomicU64 = AtomicU64::new(1_000);
        N.fetch_add(1, Ordering::SeqCst)
    }
    fn user() -> UserId {
        UserId::new(fresh())
    }
    fn guild() -> GuildId {
        GuildId::new(fresh())
    }

    fn defaults() -> NukeConfig {
        crate::state::tunables::nuke(&Default::default(), true)
    }

    fn with(f: impl FnOnce(&mut NukeConfig)) -> NukeConfig {
        let mut c = defaults();
        f(&mut c);
        c
    }

    const T0: i64 = 1_800_000_000_000;

    fn categories() -> Vec<(&'static str, usize)> {
        let c = defaults();
        vec![
            ("chDel", c.channel_delete),
            ("chCreate", c.channel_create),
            ("roleDel", c.role_delete),
            ("roleCreate", c.role_create),
            ("bans", c.ban),
            ("kicks", c.kick),
            ("webhooks", c.webhook),
            ("emojiDel", c.emoji),
        ]
    }

    #[test]
    fn one_action_and_actions_below_threshold_do_not_trip() {
        let cfg = with(|c| {
            c.channel_delete = 3;
            c.total = 0;
        });
        let (g, u) = (guild(), user());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0).is_none());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 1).is_none());
    }

    #[test]
    fn the_exact_threshold_trips_and_resets_the_counters() {
        let cfg = with(|c| c.total = 0);
        let (g, u) = (guild(), user());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0).is_none());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 1).is_none());
        let t = bump_destructive(g, u, "chDel", 3, &cfg, T0 + 2).expect("third action trips");
        assert_eq!(t.trip, Trip::Category);
        drop(t);
        // After the response, counting starts over: one more is not a trip.
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 3).is_none());
    }

    #[test]
    fn events_outside_the_window_expire() {
        let cfg = with(|c| {
            c.window_ms = 10_000;
            c.total = 0;
        });
        let (g, u) = (guild(), user());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0).is_none());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 1).is_none());
        // 10s later the first two have aged out.
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 10_001).is_none());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 10_002).is_none());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 10_003).is_some());
    }

    #[test]
    fn a_late_event_from_an_old_window_is_not_counted() {
        let cfg = with(|c| {
            c.window_ms = 10_000;
            c.total = 0;
        });
        let (g, u) = (guild(), user());
        assert!(bump_destructive(g, u, "chDel", 2, &cfg, T0 + 60_000).is_none());
        // Replayed after a reconnect, a minute stale: it can't combine with
        // the current one to trip.
        assert!(bump_destructive(g, u, "chDel", 2, &cfg, T0).is_none());
    }

    #[test]
    fn out_of_order_events_in_the_same_window_still_count() {
        let cfg = with(|c| c.total = 0);
        let (g, u) = (guild(), user());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 500).is_none());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 100).is_none());
        assert!(bump_destructive(g, u, "chDel", 3, &cfg, T0 + 300).is_some());
    }

    /// Rotating through categories used to keep every per-category counter
    /// under its own limit, so an attacker got the sum of all of them for free.
    #[test]
    fn rotating_categories_trips_on_the_shared_counter() {
        let cats = categories();
        let (g, u) = (guild(), user());
        let mut landed = 0;
        let mut trip = None;
        for i in 0..100 {
            let (key, threshold) = cats[i % cats.len()];
            landed += 1;
            trip = bump_destructive(g, u, key, threshold, &defaults(), T0 + i as i64);
            if trip.is_some() {
                break;
            }
        }
        assert_eq!(
            trip.map(|t| t.trip),
            Some(Trip::Total),
            "a rotating attack must trip the shared counter"
        );
        assert_eq!(
            landed,
            defaults().total,
            "no more actions may land than the shared threshold allows"
        );
    }

    #[test]
    fn single_category_burst_still_trips_its_own_counter() {
        let (g, u) = (guild(), user());
        let mut landed = 0;
        for i in 0..100 {
            landed += 1;
            if bump_destructive(
                g,
                u,
                "chDel",
                defaults().channel_delete,
                &defaults(),
                T0 + i,
            )
            .is_some()
            {
                break;
            }
        }
        assert_eq!(landed, defaults().channel_delete.min(defaults().total));
    }

    #[test]
    fn the_shared_counter_binds_when_a_category_is_unlimited() {
        let (g, u) = (guild(), user());
        let mut landed = 0;
        for i in 0..100 {
            landed += 1;
            if let Some(t) = bump_destructive(g, u, "chDel", usize::MAX, &defaults(), T0 + i) {
                assert_eq!(t.trip, Trip::Total);
                break;
            }
        }
        assert_eq!(landed, defaults().total);
    }

    #[test]
    fn a_zero_total_disables_the_shared_counter() {
        let cfg = with(|c| c.total = 0);
        let (g, u) = (guild(), user());
        for (i, (key, _)) in categories().iter().enumerate() {
            assert!(bump_destructive(g, u, key, usize::MAX, &cfg, T0 + i as i64).is_none());
        }
        assert!(!tracker()[&g].users[&u].counts.contains_key(TOTAL_KEY));
    }

    /// Each event lands in its category and the total exactly once, so the
    /// two stay in step.
    #[test]
    fn category_and_total_counts_stay_in_step() {
        let (g, u) = (guild(), user());
        for i in 0..3 {
            bump_destructive(g, u, "chDel", 99, &with(|c| c.total = 99), T0 + i);
            bump_destructive(g, u, "bans", 99, &with(|c| c.total = 99), T0 + 10 + i);
        }
        let map = tracker();
        let counts = &map[&g].users[&u].counts;
        assert_eq!(counts["chDel"].len(), 3);
        assert_eq!(counts["bans"].len(), 3);
        assert_eq!(counts[TOTAL_KEY].len(), 6);
    }

    /// A category trip wipes every counter, so the actions it counted can't
    /// help trip the total again straight after.
    #[test]
    fn a_trip_resets_every_counter_for_the_user() {
        let cfg = with(|c| c.total = 5);
        let (g, u) = (guild(), user());
        bump_destructive(g, u, "bans", 99, &cfg, T0);
        bump_destructive(g, u, "bans", 99, &cfg, T0 + 1);
        let t = bump_destructive(g, u, "chDel", 1, &cfg, T0 + 2).expect("category trip");
        assert!(
            tracker()[&g].users[&u].counts.is_empty(),
            "the bans went with it"
        );
        assert!(
            bump_destructive(g, u, "chDel", 1, &cfg, T0 + 3).is_none(),
            "ignored while responding"
        );
        drop(t);
        assert!(tracker()[&g].users[&u].counts.is_empty());
    }

    /// A nuke bot fires in parallel. However many events race in, exactly one
    /// response starts while the first is still running.
    #[test]
    fn a_concurrent_burst_trips_exactly_once() {
        for _ in 0..50 {
            let (g, u) = (guild(), user());
            let held = Arc::new(Mutex::new(Vec::new()));
            let threads: Vec<_> = (0..32)
                .map(|i| {
                    let held = Arc::clone(&held);
                    std::thread::spawn(move || {
                        if let Some(t) = bump_destructive(g, u, "chDel", 3, &defaults(), T0 + i) {
                            held.lock().unwrap().push(t);
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(held.lock().unwrap().len(), 1, "one burst, one response");
        }
    }

    #[test]
    fn users_and_guilds_are_counted_separately_even_concurrently() {
        let guilds = [guild(), guild()];
        let users = [user(), user()];
        let trips = Arc::new(AtomicU64::new(0));
        let threads: Vec<_> = guilds
            .iter()
            .flat_map(|g| users.iter().map(move |u| (*g, *u)))
            .map(|(g, u)| {
                let trips = Arc::clone(&trips);
                std::thread::spawn(move || {
                    let cfg = with(|c| c.total = 0);
                    // Two each: under a threshold of 3 in every (guild, user).
                    for i in 0..2 {
                        if bump_destructive(g, u, "chDel", 3, &cfg, T0 + i).is_some() {
                            trips.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(
            trips.load(Ordering::SeqCst),
            0,
            "no pair may borrow another's count"
        );
    }

    #[test]
    fn a_response_in_progress_ignores_further_events_and_releases_on_drop() {
        let cfg = with(|c| c.total = 0);
        let (g, u) = (guild(), user());
        bump_destructive(g, u, "chDel", 2, &cfg, T0);
        let t = bump_destructive(g, u, "chDel", 2, &cfg, T0 + 1).unwrap();
        assert!(responding(g, u));
        for i in 0..10 {
            assert!(bump_destructive(g, u, "chDel", 2, &cfg, T0 + 2 + i).is_none());
        }
        drop(t);
        assert!(!responding(g, u));
        assert!(
            bump_destructive(g, u, "chDel", 2, &cfg, T0 + 20).is_none(),
            "starts from zero again"
        );
    }

    /// Entries for actions taken before a response finished are late news,
    /// not a second attack.
    #[test]
    fn entries_from_before_a_response_ended_cannot_trip_again() {
        let cfg = with(|c| c.total = 0);
        let (g, u) = (guild(), user());
        bump_destructive(g, u, "chDel", 2, &cfg, T0);
        let t = bump_destructive(g, u, "chDel", 2, &cfg, T0 + 1).unwrap();
        // Arrive while the ban is going out.
        bump_destructive(g, u, "chDel", 2, &cfg, T0 + 5);
        drop(t);
        // Arrive after it, but describe actions from before it ended.
        for at in [T0 + 2, T0 + 3, T0 + 4, T0 + 5] {
            assert!(bump_destructive(g, u, "chDel", 2, &cfg, at).is_none());
        }
        assert!(tracker()[&g].users[&u].counts.is_empty());
        // New actions after that are counted as normal.
        assert!(bump_destructive(g, u, "chDel", 2, &cfg, T0 + 6).is_none());
        assert!(bump_destructive(g, u, "chDel", 2, &cfg, T0 + 7).is_some());
    }

    /// A panicking response must not leave the user marked forever.
    #[test]
    fn a_panicking_response_still_releases() {
        let cfg = with(|c| c.total = 0);
        let (g, u) = (guild(), user());
        let t = bump_destructive(g, u, "chDel", 1, &cfg, T0).unwrap();
        let r = std::thread::spawn(move || {
            let _held = t;
            panic!("response blew up");
        })
        .join();
        assert!(r.is_err());
        assert!(!responding(g, u));
    }

    #[test]
    fn duplicate_audit_entries_are_dropped() {
        let g = guild();
        let id = fresh();
        assert!(first_sighting(g, id));
        assert!(
            !first_sighting(g, id),
            "the same entry must not count twice"
        );
        assert!(first_sighting(g, fresh()));
    }

    #[test]
    fn the_seen_list_is_bounded() {
        let g = guild();
        for _ in 0..SEEN_PER_GUILD * 2 {
            first_sighting(g, fresh());
        }
        let map = tracker();
        assert_eq!(map[&g].seen.len(), SEEN_PER_GUILD);
        assert_eq!(map[&g].seen_order.len(), SEEN_PER_GUILD);
    }

    #[test]
    fn event_time_reads_the_snowflake() {
        // Discord's documented example: 175928847299117063 -> 2016-04-30 11:18:25.796 UTC.
        assert_eq!(event_time(175_928_847_299_117_063), 1_462_015_105_796);
    }

    #[test]
    fn each_guild_is_judged_by_its_own_thresholds() {
        let (a, b) = (guild(), guild());
        guild_settings::update(&a.to_string(), |s| {
            s.thresholds
                .insert(Tunable::NukeChannelDelete.key().into(), 2);
            s.thresholds.insert(Tunable::NukeTotal.key().into(), 0);
        });
        guild_settings::update(&b.to_string(), |s| {
            s.thresholds
                .insert(Tunable::NukeChannelDelete.key().into(), 5);
            s.thresholds.insert(Tunable::NukeTotal.key().into(), 0);
        });
        let (ca, cb) = (
            guild_settings::nuke(&a.to_string()),
            guild_settings::nuke(&b.to_string()),
        );
        let u = user();
        assert!(bump_destructive(a, u, "chDel", ca.channel_delete, &ca, T0).is_none());
        assert!(bump_destructive(b, u, "chDel", cb.channel_delete, &cb, T0).is_none());
        assert_eq!(
            bump_destructive(a, u, "chDel", ca.channel_delete, &ca, T0 + 1).map(|t| t.trip),
            Some(Trip::Category)
        );
        for i in 0..3 {
            assert!(bump_destructive(b, u, "chDel", cb.channel_delete, &cb, T0 + 2 + i).is_none());
        }
        assert_eq!(
            bump_destructive(b, u, "chDel", cb.channel_delete, &cb, T0 + 9).map(|t| t.trip),
            Some(Trip::Category)
        );
    }

    /// Overrides are clamped, so no stored value can make a threshold of 0
    /// (trip on nothing) or a window of 0 (never count).
    #[test]
    fn overrides_cannot_create_impossible_thresholds() {
        let g = guild();
        guild_settings::update(&g.to_string(), |s| {
            s.thresholds
                .insert(Tunable::NukeChannelDelete.key().into(), 0);
            s.thresholds.insert(Tunable::NukeWindowSec.key().into(), -5);
            s.thresholds
                .insert(Tunable::NukeTotal.key().into(), 1_000_000);
        });
        let c = guild_settings::nuke(&g.to_string());
        assert_eq!(c.channel_delete, 1);
        assert_eq!(c.window_ms, 1_000);
        assert_eq!(c.total, 500);
    }

    /// Lowering a threshold mid-window takes effect on the next event, and
    /// raising it never loses what was already counted.
    #[test]
    fn a_config_change_mid_window_is_deterministic() {
        let (g, u) = (guild(), user());
        let loose = with(|c| c.total = 0);
        for i in 0..3 {
            assert!(bump_destructive(g, u, "chDel", 10, &loose, T0 + i).is_none());
        }
        assert!(bump_destructive(g, u, "chDel", 4, &loose, T0 + 3).is_some());
    }

    #[test]
    fn anti_nuke_can_be_off_in_one_guild_and_on_in_another() {
        let (off, on) = (guild(), guild());
        guild_settings::update(&off.to_string(), |s| s.antinuke_disabled = true);
        assert!(!guild_settings::nuke(&off.to_string()).enabled);
        assert!(guild_settings::nuke(&on.to_string()).enabled);
    }

    #[test]
    fn forgetting_a_guild_drops_only_its_state_and_a_live_guard_survives_it() {
        let (leave, stay, u) = (guild(), guild(), user());
        let t = bump_destructive(leave, u, "chDel", 1, &with(|c| c.total = 0), T0);
        bump_destructive(stay, u, "chDel", 99, &defaults(), T0);
        forget_guild(leave);
        assert!(!tracker().contains_key(&leave));
        assert!(tracker().contains_key(&stay));
        drop(t); // must not panic or recreate the guild
        assert!(!tracker().contains_key(&leave));
    }

    #[test]
    fn sweep_drops_idle_users_but_never_one_being_handled() {
        let (g, idle, busy) = (guild(), user(), user());
        bump_destructive(g, idle, "chDel", 99, &defaults(), T0);
        let t = bump_destructive(g, busy, "chDel", 1, &with(|c| c.total = 0), T0);
        {
            let mut map = tracker();
            let gs = map.get_mut(&g).unwrap();
            gs.users.get_mut(&idle).unwrap().touched = 0;
            gs.users.get_mut(&busy).unwrap().touched = 0;
        }
        sweep();
        let map = tracker();
        assert!(!map[&g].users.contains_key(&idle));
        assert!(map[&g].users.contains_key(&busy));
        drop(map);
        drop(t);
    }

    #[test]
    fn our_own_bot_and_empty_executors_are_not_judged() {
        let me = UserId::new(42);
        assert_eq!(judged_executor(me, me), None);
        assert_eq!(judged_executor(UserId::new(7), me), Some(UserId::new(7)));
        // Another bot is judged like anyone else.
        assert_eq!(judged_executor(UserId::new(43), me), Some(UserId::new(43)));
    }

    #[test]
    fn the_id_only_whitelist_path_protects_both_owners_and_listed_users() {
        let g = guild();
        let owner = UserId::new(999);
        assert_eq!(
            whitelist_from_ids(g, owner, owner),
            WhitelistCheck::Decided(true)
        );
        assert_eq!(
            whitelist_from_ids(g, UserId::new(12345), owner),
            WhitelistCheck::Decided(false)
        );

        guild_settings::update(&g.to_string(), |s| {
            s.nuke_whitelist_user_ids.push("555".into())
        });
        assert_eq!(
            whitelist_from_ids(g, UserId::new(555), owner),
            WhitelistCheck::Decided(true)
        );

        guild_settings::update(&g.to_string(), |s| {
            s.nuke_whitelist_role_ids.push("777".into())
        });
        assert_eq!(
            whitelist_from_ids(g, UserId::new(12345), owner),
            WhitelistCheck::NeedsMember
        );
        // Id-based immunity still wins without a member lookup.
        assert_eq!(
            whitelist_from_ids(g, owner, owner),
            WhitelistCheck::Decided(true)
        );
    }

    #[test]
    fn the_bot_owner_is_immune_without_a_member_record() {
        let owner_id: UserId = crate::common::config::BOT_OWNER_IDS
            .iter()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .map(UserId::new)
            .expect("a bot owner is always configured");
        assert_eq!(
            whitelist_from_ids(guild(), owner_id, UserId::new(999)),
            WhitelistCheck::Decided(true)
        );
    }

    #[test]
    fn escalation_covers_updates_and_new_roles_but_not_harmless_changes() {
        let change = |old: Permissions, new: Permissions| {
            vec![Change::Permissions {
                old: Some(old),
                new: Some(new),
            }]
        };
        let update = Action::Role(RoleAction::Update);
        let create = Action::Role(RoleAction::Create);
        let admin = Permissions::ADMINISTRATOR;
        let chat = Permissions::SEND_MESSAGES;

        assert!(escalation(&update, Some(&change(chat, chat | admin))).is_some());
        // Creating a role that already has Administrator is the same thing.
        assert!(escalation(&create, Some(&change(Permissions::empty(), admin))).is_some());
        assert!(escalation(&create, Some(&change(Permissions::empty(), chat))).is_none());
        // Keeping or removing a dangerous permission isn't an escalation.
        assert!(escalation(&update, Some(&change(admin, admin | chat))).is_none());
        assert!(escalation(&update, Some(&change(admin, chat))).is_none());
        assert!(escalation(
            &Action::Role(RoleAction::Delete),
            Some(&change(chat, admin))
        )
        .is_none());
        assert!(escalation(&update, None).is_none());
    }

    #[test]
    fn a_permission_rollback_needs_a_role_it_can_edit() {
        assert!(should_revert_escalation(123, true));
        assert!(!should_revert_escalation(0, true));
        assert!(!should_revert_escalation(123, false));
    }

    #[test]
    fn only_server_errors_and_dropped_connections_are_retried() {
        assert!(transient_status(500));
        assert!(transient_status(503));
        assert!(
            !transient_status(403),
            "missing permissions fail the same way twice"
        );
        assert!(!transient_status(404));
        assert!(!transient_status(400));
    }

    #[test]
    fn unknown_member_means_not_in_the_server() {
        assert!(gone_status(404, 10007));
        assert!(gone_status(400, 10013));
        assert!(
            !gone_status(403, 50013),
            "missing permissions is not absence"
        );
    }

    fn outcome(
        ban: Option<&str>,
        stripped: usize,
        strip_error: Option<&str>,
        kick: Kick,
    ) -> Outcome {
        Outcome {
            ban_error: ban.map(str::to_string),
            stripped: (1..=stripped as u64).map(RoleId::new).collect(),
            strip_error: strip_error.map(str::to_string),
            kick,
        }
    }

    #[test]
    fn a_successful_ban_is_contained_and_says_so() {
        let o = outcome(None, 0, None, Kick::NotNeeded);
        assert!(o.contained());
        assert_eq!(o.what_i_did(), "banned them straight away.");
    }

    #[test]
    fn a_failed_ban_falls_back_to_kick_and_reports_each_step() {
        let o = outcome(Some("Missing Permissions"), 2, None, Kick::Kicked);
        assert!(o.contained());
        let s = o.what_i_did();
        assert!(s.contains("couldn't ban them (Missing Permissions)"));
        assert!(s.contains("<@&1>, <@&2>"));
        assert!(s.contains("Kicked them instead."));
    }

    #[test]
    fn a_failed_kick_and_role_removal_is_not_contained() {
        let o = outcome(
            Some("Missing Permissions"),
            1,
            Some("Missing Permissions"),
            Kick::Failed("Missing Permissions".into()),
        );
        assert!(!o.contained());
        let s = o.what_i_did();
        assert!(s.contains("Couldn't pull their dangerous roles"));
        assert!(s.contains("check my role position"));
    }

    #[test]
    fn someone_who_already_left_is_contained() {
        let o = outcome(Some("Unknown"), 0, None, Kick::NotInServer);
        assert!(o.contained());
        assert!(o.what_i_did().contains("no longer in the server"));
    }

    #[test]
    fn detectors_name_their_own_threshold() {
        let cfg = with(|c| {
            c.channel_delete = 4;
            c.window_ms = 12_000;
        });
        let d = simple_detector(&Action::Channel(ChannelAction::Delete), &cfg).unwrap();
        assert_eq!((d.key, d.threshold), ("chDel", 4));
        assert_eq!(d.reason(&cfg), "Deleted 4+ channels in 12s");
        let d = simple_detector(&Action::Webhook(WebhookAction::Create), &cfg).unwrap();
        assert_eq!(d.key, "webhooks");
        assert!(simple_detector(&Action::GuildUpdate, &cfg).is_none());
    }
}
