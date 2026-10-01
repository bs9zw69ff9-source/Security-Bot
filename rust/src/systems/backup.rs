//! `/backup`: Xenon-style server backups. Server owner and bot owners only.
//!
//! A backup belongs to the user who took it and can be loaded into any server
//! that user owns, so it doubles as a way to clone a server. Loading edits
//! same-named roles and channels in place, creates what's missing and, if asked,
//! deletes what isn't in the backup.

use once_cell::sync::Lazy;
use serenity::builder::{
    CreateActionRow, CreateAttachment, CreateAutocompleteResponse, CreateButton, CreateChannel, CreateEmbed,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateAllowedMentions, CreateInvite, CreateMessage, CreateWebhook, ExecuteWebhook, GetMessages, EditChannel, EditGuild, EditInteractionResponse,
    EditMember, EditRole,
};
use serenity::client::Context;
use serenity::collector::ComponentInteractionCollector;
use serenity::http::{LightMethod, Request, Route};
use serenity::model::application::{ButtonStyle, CommandInteraction, ResolvedOption, ResolvedValue};
use serenity::model::channel::{MessageType, PermissionOverwrite, PermissionOverwriteType};
use serenity::model::guild::{AfkTimeout, DefaultMessageNotificationLevel, ExplicitContentFilter, VerificationLevel};
use serenity::model::id::{ChannelId, GuildId, RoleId, UserId};
use serenity::model::Permissions;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::common::config::now_ms;
use crate::common::guildinfo::{all_members, fetch_member, GuildInfo};
use crate::common::permissions::try_dm_embed;
use crate::common::theme::{self, Tone};
use crate::state::backups::{self, *};
use crate::systems::snapshot_rollback::{channel_kind_num, kind_from_num};

const OWNER_ONLY: &str = "Backups are for the server owner (and bot owners) only.";

/// Which backups this user can reach: bot owners reach every backup, everyone
/// else only their own.
fn scope(user: &str) -> Option<&str> {
    (!crate::common::permissions::is_owner_str(user)).then_some(user)
}

/// guild -> cancel flag of the load running there.
static LOADS: Lazy<Mutex<HashMap<GuildId, Arc<AtomicBool>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn loads() -> std::sync::MutexGuard<'static, HashMap<GuildId, Arc<AtomicBool>>> {
    LOADS.lock().unwrap_or_else(|e| e.into_inner())
}

// ── Taking a backup ───────────────────────────────────────────

fn overwrites_of(ows: &[PermissionOverwrite]) -> Vec<BOverwrite> {
    ows.iter()
        .filter_map(|o| {
            let (id, kind) = match o.kind {
                PermissionOverwriteType::Role(r) => (r.to_string(), 0),
                PermissionOverwriteType::Member(m) => (m.to_string(), 1),
                _ => return None,
            };
            Some(BOverwrite { id, kind, allow: o.allow.bits().to_string(), deny: o.deny.bits().to_string() })
        })
        .collect()
}

/// The last `limit` user messages in a channel, oldest first.
async fn recent_messages(ctx: &Context, channel: ChannelId, limit: usize) -> serenity::Result<Vec<BMessage>> {
    let mut out = Vec::new();
    let mut before = None;
    while out.len() < limit {
        let mut req = GetMessages::new().limit(100);
        if let Some(b) = before {
            req = req.before(b);
        }
        let page = channel.messages(&ctx.http, req).await?;
        let last_page = page.len() < 100;
        before = page.last().map(|m| m.id);
        for m in page {
            if !matches!(m.kind, MessageType::Regular | MessageType::InlineReply) {
                continue;
            }
            let embeds: Vec<_> = m.embeds.into_iter().filter(|e| e.kind.as_deref() == Some("rich")).collect();
            if m.content.is_empty() && embeds.is_empty() && m.attachments.is_empty() {
                continue;
            }
            out.push(BMessage {
                author: m.author.global_name.clone().unwrap_or_else(|| m.author.name.clone()),
                avatar: m.author.face(),
                content: m.content,
                embeds,
                attachments: m.attachments.into_iter().map(|a| BAttachment { name: a.filename, url: a.url }).collect(),
                at: m.timestamp.unix_timestamp(),
                pinned: m.pinned,
            });
            if out.len() == limit {
                break;
            }
        }
        if last_page {
            break;
        }
    }
    out.reverse();
    Ok(out)
}

/// Webhook names can't contain "discord" or "clyde" and must be 1-80 chars.
fn webhook_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let clean = if lower.contains("discord") || lower.contains("clyde") || name.trim().is_empty() {
        "Former member".to_string()
    } else {
        name.trim().to_string()
    };
    clean.chars().take(80).collect()
}

/// Message text as it's replayed: the original, its attachments as links
/// (the files themselves aren't kept), and the original date underneath.
fn replay_text(m: &BMessage) -> String {
    let stamp = format!("\n-# <t:{}:f>", m.at);
    let mut body = replay_body(m);
    let room = 2000 - stamp.chars().count();
    if body.chars().count() > room {
        body = body.chars().take(room - 1).collect::<String>() + "…";
    }
    body + &stamp
}

fn replay_body(m: &BMessage) -> String {
    let mut body = m.content.clone();
    for a in &m.attachments {
        body.push_str(&format!("\n📎 [{}]({})", a.name, a.url));
    }
    body
}

/// What identifies a message across a replay: its original second and the
/// start of its text. A replayed copy carries the original second in its
/// date line, so an original and its copy get the same key.
fn message_key(m: &BMessage) -> (i64, String) {
    let replayed = m.content.rfind("\n-# <t:").and_then(|at| {
        let stamp = &m.content[at + "\n-# <t:".len()..];
        let secs = stamp.split(':').next()?.parse::<i64>().ok()?;
        Some((secs, m.content[..at].to_string()))
    });
    let (secs, body) = replayed.unwrap_or_else(|| (m.at, replay_body(m)));
    (secs, body.chars().take(30).collect())
}

/// How far back to look for messages that are already there. Wider than
/// what's saved, so chat since the backup doesn't hide the originals.
const DEDUPE_DEPTH: usize = 1000;

/// Read a server into a backup. Channels are required; bans and role
/// assignments are best-effort and reported as warnings when they can't be read.
pub async fn capture(ctx: &Context, guild_id: GuildId, owner: UserId, interval: bool) -> Result<(Backup, Vec<String>), String> {
    let channels = guild_id.channels(&ctx.http).await.map_err(|e| format!("I couldn't read the channel list: {e}"))?;
    let mut warnings = Vec::new();

    let (settings, roles, emojis) = {
        let g = ctx.cache.guild(guild_id).ok_or("I haven't finished loading this server yet.")?;
        let settings = BSettings {
            name: g.name.to_string(),
            icon_url: g.icon_url(),
            verification_level: u8::from(g.verification_level),
            default_notifications: u8::from(g.default_message_notifications),
            explicit_content_filter: u8::from(g.explicit_content_filter),
            afk_channel: g.afk_metadata.as_ref().map(|a| a.afk_channel_id.to_string()),
            afk_timeout: g.afk_metadata.as_ref().map(|a| u16::from(a.afk_timeout)).unwrap_or(300),
            system_channel: g.system_channel_id.map(|c| c.to_string()),
            everyone_permissions: g
                .roles
                .get(&guild_id.everyone_role())
                .map(|r| r.permissions.bits().to_string())
                .unwrap_or_default(),
        };
        let mut roles: Vec<BRole> = g
            .roles
            .values()
            .filter(|r| r.id != guild_id.everyone_role() && !r.managed)
            .map(|r| BRole {
                id: r.id.to_string(),
                name: r.name.to_string(),
                color: r.colour.0,
                hoist: r.hoist,
                mentionable: r.mentionable,
                permissions: r.permissions.bits().to_string(),
                position: r.position as i64,
            })
            .collect();
        roles.sort_by_key(|r| r.position);
        let emojis: Vec<BEmoji> =
            g.emojis.values().filter(|e| !e.managed).map(|e| BEmoji { name: e.name.clone(), url: e.url() }).collect();
        (settings, roles, emojis)
    };

    let mut chans: Vec<BChannel> = channels
        .values()
        .map(|c| BChannel {
            id: c.id.to_string(),
            name: c.name.clone(),
            kind: channel_kind_num(c.kind),
            parent_id: c.parent_id.map(|p| p.to_string()),
            position: c.position as i64,
            topic: c.topic.clone(),
            nsfw: c.nsfw,
            rate_limit: c.rate_limit_per_user.unwrap_or(0),
            bitrate: c.bitrate,
            user_limit: c.user_limit,
            overwrites: overwrites_of(&c.permission_overwrites),
            messages: Vec::new(),
        })
        .collect();
    chans.sort_by_key(|c| c.position);

    let mut unreadable = 0;
    for c in chans.iter_mut().filter(|c| matches!(c.kind, 0 | 5)) {
        match recent_messages(ctx, ChannelId::new(id_of(&c.id).unwrap_or(1)), MESSAGES_PER_CHANNEL).await {
            Ok(m) => c.messages = m,
            Err(_) => unreadable += 1,
        }
    }
    if unreadable > 0 {
        warnings.push(format!("Messages weren't saved from {unreadable} channel(s) I can't read."));
    }

    let mut bans = Vec::new();
    let mut after = None;
    loop {
        match guild_id.bans(&ctx.http, after, Some(250)).await {
            Ok(page) => {
                let full = page.len() == 250;
                after = page.last().map(|b| serenity::http::UserPagination::After(b.user.id));
                bans.extend(page.into_iter().map(|b| BBan { user_id: b.user.id.to_string(), reason: b.reason }));
                if !full {
                    break;
                }
            }
            Err(e) => {
                warnings.push(format!("Bans weren't saved: {e}"));
                bans.clear();
                break;
            }
        }
    }

    let role_ids: HashSet<String> = roles.iter().map(|r| r.id.clone()).collect();
    let members = match all_members(ctx, guild_id).await {
        Ok(ms) => ms
            .into_iter()
            .filter(|m| !m.user.bot)
            .map(|m| BMember {
                user_id: m.user.id.to_string(),
                nick: m.nick,
                roles: m.roles.iter().map(|r| r.to_string()).filter(|r| role_ids.contains(r)).collect(),
            })
            .collect(),
        Err(e) => {
            warnings.push(format!("Role assignments weren't saved: {e}"));
            Vec::new()
        }
    };

    let counts = Counts {
        roles: roles.len(),
        channels: chans.len(),
        emojis: emojis.len(),
        bans: bans.len(),
        members: members.len(),
        messages: chans.iter().map(|c| c.messages.len()).sum(),
    };
    let backup = Backup {
        id: new_id(),
        owner_id: owner.to_string(),
        guild_id: guild_id.to_string(),
        guild_name: settings.name.clone(),
        created_at: now_ms(),
        interval,
        counts,
        settings,
        roles,
        channels: chans,
        emojis,
        bans,
        members,
    };
    Ok((backup, warnings))
}

// ── Loading a backup ──────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct LoadOptions {
    pub settings: bool,
    pub roles: bool,
    pub channels: bool,
    pub delete_roles: bool,
    pub delete_channels: bool,
    pub emojis: bool,
    pub bans: bool,
    pub members: bool,
    pub dm_invite: bool,
    pub messages: bool,
    pub ban_members: bool,
}

/// Who `ban_members` leaves in place: bots, the server owner (Discord won't
/// allow it anyway), whoever ran the load, and bot owners.
fn spared_from_ban(user: UserId, is_bot: bool, guild_owner: Option<UserId>, invoker: UserId) -> bool {
    is_bot || Some(user) == guild_owner || user == invoker || crate::common::permissions::is_owner(user)
}

#[derive(Default)]
struct Tally {
    created: usize,
    edited: usize,
    deleted: usize,
    failed: usize,
}

impl Tally {
    fn line(&self, what: &str) -> String {
        let mut s = format!("**{what}:** {} created, {} updated, {} deleted", self.created, self.edited, self.deleted);
        if self.failed > 0 {
            s.push_str(&format!(", **{} failed**", self.failed));
        }
        s
    }
}

fn perms(bits: &str) -> Permissions {
    Permissions::from_bits_truncate(bits.parse().unwrap_or(0))
}

fn id_of(raw: &str) -> Option<u64> {
    raw.parse::<u64>().ok().filter(|n| *n != 0)
}

/// Discord only accepts these AFK timeouts.
fn afk_timeout(secs: u16) -> AfkTimeout {
    match secs {
        0..=60 => AfkTimeout::OneMinute,
        61..=300 => AfkTimeout::FiveMinutes,
        301..=900 => AfkTimeout::FifteenMinutes,
        901..=1800 => AfkTimeout::ThirtyMinutes,
        _ => AfkTimeout::OneHour,
    }
}

struct Loader<'a> {
    ctx: &'a Context,
    i: &'a CommandInteraction,
    guild_id: GuildId,
    cancel: Arc<AtomicBool>,
    lines: Vec<String>,
}

impl Loader<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    async fn progress(&self, step: &str) {
        let mut body = self.lines.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("⏳ {step}…\n\n_`/backup cancel` stops it; what's done stays done._"));
        let card = theme::card(Tone::Info, Some("Loading backup"), body);
        let _ = self.i.edit_response(&self.ctx.http, EditInteractionResponse::new().embed(card).components(vec![])).await;
    }
}

/// How many requests of each kind run at once. Discord's per-route limits
/// still apply and serenity waits them out, so this only sets how many are
/// kept in flight; the global limit is 50 requests a second.
const FAST: usize = 24;
/// Channels replayed at once. Each has its own webhook and its own limit.
const REPLAYS: usize = 8;
/// Invite DMs at once. DMs are limited far more tightly than anything else.
const DMS: usize = 4;
/// Users per bulk-ban request, Discord's maximum.
const BULK_BAN: usize = 200;

/// Run `f` over `items`, up to `limit` at a time, in no particular order.
///
/// Boxed and spelled out with `Send` bounds: left to inference, the compiler
/// can't prove the borrowed futures `Send` for every lifetime.
fn par<'a, I, R, F, Fut>(items: I, limit: usize, f: F) -> BoxFuture<'a, Vec<R>>
where
    I: IntoIterator + Send + 'a,
    I::IntoIter: Send,
    I::Item: Send + 'a,
    R: Send + 'a,
    F: FnMut(I::Item) -> Fut + Send + 'a,
    Fut: std::future::Future<Output = R> + Send + 'a,
{
    futures::stream::iter(items).map(f).buffer_unordered(limit).collect().boxed()
}

struct Env<'a> {
    ctx: &'a Context,
    gid: GuildId,
    cancel: &'a AtomicBool,
    reason: String,
}

impl Env<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

fn failed_note(failed: usize, why: &str) -> String {
    if failed > 0 {
        format!(", **{failed} {why}**")
    } else {
        String::new()
    }
}

async fn delete_roles(env: &Env<'_>, ids: &[u64]) -> Tally {
    let done = par(ids, FAST, |id| async move {
        !env.cancelled() && env.gid.delete_role(&env.ctx.http, RoleId::new(*id)).await.is_ok()
    })
    .await;
    let deleted = done.iter().filter(|ok| **ok).count();
    Tally { deleted, failed: if env.cancelled() { 0 } else { done.len() - deleted }, ..Tally::default() }
}

async fn delete_channels(env: &Env<'_>, ids: &[u64]) -> Tally {
    // Children first would only matter for ordering; deleting a category
    // leaves its children in place, so everything can go at once.
    let done = par(ids, FAST, |id| async move {
        !env.cancelled() && ChannelId::new(*id).delete(&env.ctx.http).await.is_ok()
    })
    .await;
    let deleted = done.iter().filter(|ok| **ok).count();
    Tally { deleted, failed: if env.cancelled() { 0 } else { done.len() - deleted }, ..Tally::default() }
}

/// Ban `users` in bulk requests of 200. Returns who was banned and how many
/// weren't.
async fn ban_many(env: &Env<'_>, users: &[UserId], reason: &str) -> (Vec<UserId>, usize) {
    let results = par(users.chunks(BULK_BAN), 4, |chunk| async move {
        if env.cancelled() {
            return (Vec::new(), 0);
        }
        match env.gid.bulk_ban(&env.ctx.http, chunk, 0, Some(reason)).await {
            Ok(r) => (r.banned_users, r.failed_users.len()),
            Err(_) => (Vec::new(), chunk.len()),
        }
    })
    .await;
    results.into_iter().fold((Vec::new(), 0), |(mut banned, failed), (b, f)| {
        banned.extend(b);
        (banned, failed + f)
    })
}

/// `ban_members`: everyone currently in the server except those spared.
async fn ban_current(env: &Env<'_>, invoker: UserId) -> (HashSet<UserId>, String) {
    let current = match all_members(env.ctx, env.gid).await {
        Ok(m) => m,
        Err(e) => return (HashSet::new(), format!("⚠️ **Bans:** skipped, I couldn't read the member list: {e}")),
    };
    let owner = env.ctx.cache.guild(env.gid).map(|g| g.owner_id);
    let targets: Vec<UserId> = current
        .iter()
        .filter(|m| !spared_from_ban(m.user.id, m.user.bot, owner, invoker))
        .map(|m| m.user.id)
        .collect();
    let (banned, failed) = ban_many(env, &targets, &env.reason).await;
    let line = format!(
        "✅ **Bans:** banned {} current members{}",
        banned.len(),
        failed_note(failed, "couldn't be banned** (above me, or already gone")
    );
    (banned.into_iter().collect(), line)
}

async fn restore_bans(env: &Env<'_>, b: &Backup) -> String {
    let users: Vec<UserId> = b.bans.iter().filter_map(|x| id_of(&x.user_id).map(UserId::new)).collect();
    let (banned, failed) = ban_many(env, &users, "Restored from a backup").await;
    format!("✅ **Bans:** {} restored{}", banned.len(), failed_note(failed, "failed"))
}

async fn restore_emojis(env: &Env<'_>, b: &Backup) -> String {
    let existing: HashSet<String> =
        env.ctx.cache.guild(env.gid).map(|g| g.emojis.values().map(|e| e.name.clone()).collect()).unwrap_or_default();
    let done = par(b.emojis.iter().filter(|e| !existing.contains(&e.name)), 6, |e| async move {
        if env.cancelled() {
            return true;
        }
        match CreateAttachment::url(&env.ctx.http, &e.url).await {
            Ok(img) => env.gid.create_emoji(&env.ctx.http, &e.name, &img.to_base64()).await.is_ok(),
            Err(_) => false,
        }
    })
    .await;
    let made = done.iter().filter(|ok| **ok).count();
    format!("✅ **Emojis:** {made} added{}", failed_note(done.len() - made, "failed"))
}

enum Restored<T> {
    Edited(T),
    Created(T),
    Failed,
}

/// One bulk request to set positions, instead of one request per item.
async fn patch_positions(env: &Env<'_>, route: Route<'_>, body: serde_json::Value) {
    let Ok(bytes) = serde_json::to_vec(&body) else { return };
    let req = Request::new(route, LightMethod::Patch).body(Some(bytes));
    if let Err(e) = env.ctx.http.fire::<serde_json::Value>(req).await {
        eprintln!("⚠️ [{}] backup load couldn't set positions: {e}", env.gid);
    }
}

async fn restore_roles(env: &Env<'_>, b: &Backup, reuse: &HashMap<String, RoleId>) -> (HashMap<String, RoleId>, Tally) {
    let results = par(b.roles.iter(), FAST, |r| async move {
        if env.cancelled() {
            return (&r.id, Restored::Failed);
        }
        let builder = EditRole::new()
            .name(r.name.clone())
            .colour(r.color as u64)
            .hoist(r.hoist)
            .mentionable(r.mentionable)
            .permissions(perms(&r.permissions))
            .audit_log_reason(&env.reason);
        let out = match reuse.get(&r.id) {
            Some(live) => env.gid.edit_role(&env.ctx.http, *live, builder).await.map(|_| Restored::Edited(*live)),
            None => env.gid.create_role(&env.ctx.http, builder).await.map(|role| Restored::Created(role.id)),
        };
        (&r.id, out.unwrap_or(Restored::Failed))
    })
    .await;

    let mut map = reuse.clone();
    let mut t = Tally::default();
    for (id, res) in results {
        match res {
            Restored::Edited(_) => t.edited += 1,
            Restored::Created(live) => {
                t.created += 1;
                map.insert(id.clone(), live);
            }
            Restored::Failed => t.failed += 1,
        }
    }
    // Created concurrently, so their order is whatever finished first. Fix it
    // in one request: the backup's order, bottom up.
    let order: Vec<serde_json::Value> = b
        .roles
        .iter()
        .filter_map(|r| map.get(&r.id))
        .enumerate()
        .map(|(n, live)| serde_json::json!({ "id": live, "position": n + 1 }))
        .collect();
    if !order.is_empty() && !env.cancelled() {
        patch_positions(env, Route::GuildRoles { guild_id: env.gid }, serde_json::Value::Array(order)).await;
    }
    (map, t)
}

#[allow(clippy::too_many_arguments)]
async fn restore_channel(
    env: &Env<'_>,
    c: &BChannel,
    existing: Option<ChannelId>,
    parent: Option<ChannelId>,
    overwrites: Vec<PermissionOverwrite>,
) -> Restored<ChannelId> {
    if env.cancelled() {
        return Restored::Failed;
    }
    let kind = kind_from_num(c.kind);
    let voice = matches!(c.kind, 2 | 13);
    let texty = matches!(c.kind, 0 | 5 | 15);
    let http = &env.ctx.http;
    match existing {
        Some(live) => {
            let mut e = EditChannel::new().name(c.name.clone()).permissions(overwrites);
            if c.kind != 4 {
                e = e.category(parent);
            }
            if texty {
                e = e.nsfw(c.nsfw).rate_limit_per_user(c.rate_limit).topic(c.topic.clone().unwrap_or_default());
            }
            if voice {
                if let Some(br) = c.bitrate {
                    e = e.bitrate(br);
                }
                e = e.user_limit(c.user_limit.unwrap_or(0));
            }
            match live.edit(http, e.audit_log_reason(&env.reason)).await {
                Ok(_) => Restored::Edited(live),
                Err(_) => Restored::Failed,
            }
        }
        None => {
            let mut e = CreateChannel::new(c.name.clone()).kind(kind).permissions(overwrites);
            if let Some(p) = parent {
                e = e.category(p);
            }
            if texty {
                e = e.nsfw(c.nsfw).rate_limit_per_user(c.rate_limit);
                if let Some(topic) = &c.topic {
                    e = e.topic(topic.clone());
                }
            }
            if voice {
                if let Some(br) = c.bitrate {
                    e = e.bitrate(br);
                }
                if let Some(ul) = c.user_limit {
                    e = e.user_limit(ul);
                }
            }
            match env.gid.create_channel(http, e.audit_log_reason(&env.reason)).await {
                Ok(ch) => Restored::Created(ch.id),
                Err(_) => Restored::Failed,
            }
        }
    }
}

/// Categories first, all at once, then everything inside them, all at once.
async fn restore_channels(
    env: &Env<'_>,
    b: &Backup,
    reuse: &HashMap<String, ChannelId>,
    role_map: &HashMap<String, RoleId>,
) -> (HashMap<String, ChannelId>, HashSet<ChannelId>, Tally) {
    let everyone_src = &b.guild_id;
    let remap = |ows: &[BOverwrite]| -> Vec<PermissionOverwrite> {
        ows.iter()
            .filter_map(|o| {
                let (allow, deny) = (perms(&o.allow), perms(&o.deny));
                let kind = if o.kind == 1 {
                    PermissionOverwriteType::Member(UserId::new(id_of(&o.id)?))
                } else if &o.id == everyone_src {
                    PermissionOverwriteType::Role(env.gid.everyone_role())
                } else {
                    PermissionOverwriteType::Role(*role_map.get(&o.id)?)
                };
                Some(PermissionOverwrite { allow, deny, kind })
            })
            .collect()
    };

    let mut map = reuse.clone();
    let mut created = HashSet::new();
    let mut t = Tally::default();
    let (cats, rest): (Vec<&BChannel>, Vec<&BChannel>) = b.channels.iter().partition(|c| c.kind == 4);
    for group in [cats, rest] {
        let results = par(group, FAST, |c| {
            let parent = c.parent_id.as_ref().and_then(|p| map.get(p).copied());
            let existing = map.get(&c.id).copied();
            let overwrites = remap(&c.overwrites);
            async move { (&c.id, restore_channel(env, c, existing, parent, overwrites).await) }
        })
        .await;
        for (id, res) in results {
            match res {
                Restored::Edited(_) => t.edited += 1,
                Restored::Created(live) => {
                    t.created += 1;
                    created.insert(live);
                    map.insert(id.clone(), live);
                }
                Restored::Failed => t.failed += 1,
            }
        }
    }

    // One request for the order and the parents, rather than one per channel.
    let layout: Vec<serde_json::Value> = b
        .channels
        .iter()
        .filter_map(|c| {
            let live = map.get(&c.id)?;
            let mut v = serde_json::json!({ "id": live, "position": c.position.max(0) });
            if c.kind != 4 {
                v["parent_id"] = match c.parent_id.as_ref().and_then(|p| map.get(p)) {
                    Some(p) => serde_json::json!(p),
                    None => serde_json::Value::Null,
                };
            }
            Some(v)
        })
        .collect();
    if !layout.is_empty() && !env.cancelled() {
        patch_positions(env, Route::GuildChannels { guild_id: env.gid }, serde_json::Value::Array(layout)).await;
    }
    (map, created, t)
}

async fn restore_settings(env: &Env<'_>, b: &Backup, chan_map: &HashMap<String, ChannelId>) -> String {
    let (ctx, gid, s) = (env.ctx, env.gid, &b.settings);
    let mut e = EditGuild::new()
        .name(s.name.clone())
        .verification_level(VerificationLevel::from(s.verification_level))
        .default_message_notifications(Some(DefaultMessageNotificationLevel::from(s.default_notifications)))
        .explicit_content_filter(Some(ExplicitContentFilter::from(s.explicit_content_filter)))
        .afk_timeout(afk_timeout(s.afk_timeout))
        .afk_channel(s.afk_channel.as_ref().and_then(|c| chan_map.get(c).copied()))
        .system_channel_id(s.system_channel.as_ref().and_then(|c| chan_map.get(c).copied()))
        .audit_log_reason(&env.reason);
    let icon = match &s.icon_url {
        Some(url) => CreateAttachment::url(&ctx.http, url).await.ok(),
        None => None,
    };
    if icon.is_some() {
        e = e.icon(icon.as_ref());
    }
    let everyone = EditRole::new().permissions(perms(&s.everyone_permissions));
    let (guild_edit, everyone_edit) = tokio::join!(gid.edit(&ctx.http, e), gid.edit_role(&ctx.http, gid.everyone_role(), everyone));
    let mut notes = Vec::new();
    if s.icon_url.is_some() && icon.is_none() {
        notes.push("icon couldn't be downloaded");
    }
    if guild_edit.is_err() {
        notes.push("some settings were refused");
    }
    if everyone_edit.is_err() {
        notes.push("@everyone permissions weren't changed");
    }
    if notes.is_empty() {
        "✅ **Settings:** name, icon, verification, notifications, AFK, system channel, @everyone".to_string()
    } else {
        format!("⚠️ **Settings:** restored, but {}", notes.join(", "))
    }
}

/// Replay one channel's messages in order, through its own webhook.
async fn replay_channel(env: &Env<'_>, c: &BChannel, channel: ChannelId, fresh: bool) -> (usize, usize, usize) {
    let (ctx, mut sent, mut failed) = (env.ctx, 0usize, 0usize);
    // A channel that already existed may still hold some or all of these,
    // originals or copies from an earlier load. Only the missing ones are
    // posted, so loading twice never doubles them.
    let present: HashSet<(i64, String)> = if fresh {
        HashSet::new()
    } else {
        match recent_messages(ctx, channel, DEDUPE_DEPTH).await {
            Ok(ms) => ms.iter().map(message_key).collect(),
            Err(_) => return (0, c.messages.len(), 0),
        }
    };
    let missing: Vec<&BMessage> = c.messages.iter().filter(|m| !present.contains(&message_key(m))).collect();
    let kept = c.messages.len() - missing.len();
    if missing.is_empty() {
        return (0, 0, kept);
    }
    let hook = match channel.create_webhook(&ctx.http, CreateWebhook::new("Backup restore")).await {
        Ok(h) => h,
        Err(_) => return (0, c.messages.len(), kept),
    };
    for m in missing {
        if env.cancelled() {
            break;
        }
        let mut exec = ExecuteWebhook::new()
            .username(webhook_name(&m.author))
            .avatar_url(m.avatar.clone())
            .content(replay_text(m))
            .allowed_mentions(CreateAllowedMentions::new());
        if !m.embeds.is_empty() {
            exec = exec.embeds(m.embeds.iter().take(10).cloned().map(CreateEmbed::from).collect());
        }
        match hook.execute(&ctx.http, m.pinned, exec).await {
            Ok(posted) => {
                sent += 1;
                if let Some(msg) = posted {
                    let _ = channel.pin(&ctx.http, msg.id).await;
                }
            }
            Err(_) => failed += 1,
        }
    }
    let _ = hook.delete(&ctx.http).await;
    (sent, failed, kept)
}

async fn replay_messages(
    env: &Env<'_>,
    b: &Backup,
    chan_map: &HashMap<String, ChannelId>,
    created: &HashSet<ChannelId>,
) -> String {
    let targets: Vec<(&BChannel, ChannelId)> = b
        .channels
        .iter()
        .filter(|c| !c.messages.is_empty())
        .filter_map(|c| chan_map.get(&c.id).map(|id| (c, *id)))
        .collect();
    let results = par(&targets, REPLAYS, |(c, channel)| replay_channel(env, c, *channel, created.contains(channel))).await;
    let (sent, failed, kept) = results.iter().fold((0, 0, 0), |a, r| (a.0 + r.0, a.1 + r.1, a.2 + r.2));
    format!(
        "✅ **Messages:** {sent} replayed across {} channel(s), {kept} skipped as already there{}",
        targets.len(),
        failed_note(failed, "failed")
    )
}

/// Give members their saved roles and nicknames back, many at once.
async fn restore_members(
    env: &Env<'_>,
    b: &Backup,
    live: &[serenity::model::guild::Member],
    role_map: &HashMap<String, RoleId>,
) -> String {
    let owner = env.ctx.cache.guild(env.gid).map(|g| g.owner_id);
    let wanted: HashMap<&str, &BMember> = b.members.iter().map(|m| (m.user_id.as_str(), m)).collect();
    let edits: Vec<(UserId, EditMember<'_>)> = live
        .iter()
        .filter_map(|m| {
            let saved = wanted.get(m.user.id.to_string().as_str())?;
            let missing: Vec<RoleId> =
                saved.roles.iter().filter_map(|r| role_map.get(r).copied()).filter(|r| !m.roles.contains(r)).collect();
            let nick = saved.nick.clone().filter(|n| m.nick.as_ref() != Some(n) && Some(m.user.id) != owner);
            if missing.is_empty() && nick.is_none() {
                return None;
            }
            let mut roles = m.roles.clone();
            roles.extend(missing);
            let mut e = EditMember::new().roles(roles).audit_log_reason(&env.reason);
            if let Some(n) = nick {
                e = e.nickname(n);
            }
            Some((m.user.id, e))
        })
        .collect();
    let done = par(edits, FAST, |(user, e)| async move {
        !env.cancelled() && env.gid.edit_member(&env.ctx.http, user, e).await.is_ok()
    })
    .await;
    let updated = done.iter().filter(|ok| **ok).count();
    format!("✅ **Members:** {updated} given their roles and nicknames back{}", failed_note(done.len() - updated, "failed"))
}

/// DM an invite to every saved member who isn't here, lifting this load's own
/// bans on them first so the invite works.
async fn invite_members(
    env: &Env<'_>,
    b: &Backup,
    live: &[serenity::model::guild::Member],
    banned_now: &HashSet<UserId>,
    channel: ChannelId,
) -> String {
    let (ctx, gid) = (env.ctx, env.gid);
    let here: HashSet<UserId> = live.iter().map(|m| m.user.id).collect();
    let banned: HashSet<&str> = b.bans.iter().map(|b| b.user_id.as_str()).collect();
    let missing: Vec<UserId> = b
        .members
        .iter()
        .filter(|m| !banned.contains(m.user_id.as_str()))
        .filter_map(|m| id_of(&m.user_id).map(UserId::new))
        .filter(|u| !here.contains(u))
        .collect();
    if missing.is_empty() {
        return "✅ **Invites:** everyone in the backup is already here".to_string();
    }
    let invite = match channel.create_invite(&ctx.http, CreateInvite::new().max_age(7 * 86_400).max_uses(0).unique(true)).await {
        Ok(inv) => inv.url(),
        Err(e) => return format!("⚠️ **Invites:** not sent, I couldn't make an invite: {e}"),
    };
    let server = ctx.cache.guild(gid).map(|g| g.name.to_string()).unwrap_or_default();
    let card = theme::card(
        Tone::Info,
        Some("You're invited back"),
        format!("**{}** has been restored as **{server}**. You were a member, so here's an invite (valid for 7 days):\n{invite}", b.guild_name),
    );
    // The unbans and the DMs are both per person; a few at a time.
    let results = par(missing, DMS, |user| {
        let card = card.clone();
        async move {
            if env.cancelled() {
                return (false, false, false);
            }
            let unbanned = banned_now.contains(&user);
            if unbanned && gid.unban(&ctx.http, user).await.is_err() {
                return (false, false, true);
            }
            let sent = user.direct_message(&ctx.http, CreateMessage::new().embed(card)).await.is_ok();
            (sent, unbanned, !sent)
        }
    })
    .await;
    let sent = results.iter().filter(|r| r.0).count();
    let unbanned = results.iter().filter(|r| r.1).count();
    let failed = results.iter().filter(|r| r.2).count();
    format!(
        "✅ **Invites:** DMed {sent} members an invite{}{}",
        if unbanned > 0 { format!(" (lifted {unbanned} of this load's bans so they can use it)") } else { String::new() },
        failed_note(failed, "couldn't be reached** (DMs closed or no shared server")
    )
}

async fn run_load(l: &mut Loader<'_>, b: &Backup, o: LoadOptions, plan: Plan) {
    let cancel = l.cancel.clone();
    let env = Env { ctx: l.ctx, gid: l.guild_id, cancel: &cancel, reason: format!("Backup {} loaded", b.id) };
    let invoker = l.i.user.id;

    // Deleting, banning and emojis don't depend on each other or on anything
    // restored later, so they all run together.
    l.progress("Clearing out the old server").await;
    let want_role_deletes = o.delete_roles && !plan.delete_roles.is_empty();
    let want_channel_deletes = o.delete_channels && !plan.delete_channels.is_empty();
    let (role_deletes, channel_deletes, current_bans, restored_bans, emojis) = tokio::join!(
        async { if want_role_deletes { Some(delete_roles(&env, &plan.delete_roles).await) } else { None } },
        async { if want_channel_deletes { Some(delete_channels(&env, &plan.delete_channels).await) } else { None } },
        async { if o.ban_members { Some(ban_current(&env, invoker).await) } else { None } },
        async { if o.bans && !b.bans.is_empty() { Some(restore_bans(&env, b).await) } else { None } },
        async { if o.emojis && !b.emojis.is_empty() { Some(restore_emojis(&env, b).await) } else { None } },
    );
    let (banned_now, ban_line) = match current_bans {
        Some((set, line)) => (set, Some(line)),
        None => (HashSet::new(), None),
    };
    l.lines.extend(ban_line);
    l.lines.extend(restored_bans);
    l.lines.extend(emojis);
    if l.cancelled() {
        return;
    }

    // Roles, then channels (their permissions point at roles).
    let mut role_tally = Tally::default();
    for t in [role_deletes].into_iter().flatten() {
        role_tally.deleted = t.deleted;
        role_tally.failed = t.failed;
    }
    let reuse_roles: HashMap<String, RoleId> = plan.reuse_roles.iter().map(|(k, v)| (k.clone(), RoleId::new(*v))).collect();
    let mut role_map = reuse_roles.clone();
    if o.roles {
        l.progress("Restoring roles").await;
        let (map, t) = restore_roles(&env, b, &reuse_roles).await;
        role_map = map;
        role_tally.created = t.created;
        role_tally.edited = t.edited;
        role_tally.failed += t.failed;
    }
    if o.roles || want_role_deletes {
        l.lines.push(format!("✅ {}", role_tally.line("Roles")));
    }
    if l.cancelled() {
        return;
    }

    let mut chan_tally = channel_deletes.unwrap_or_default();
    let reuse_channels: HashMap<String, ChannelId> =
        plan.reuse_channels.iter().map(|(k, v)| (k.clone(), ChannelId::new(*v))).collect();
    let mut chan_map = reuse_channels.clone();
    let mut created_channels = HashSet::new();
    if o.channels {
        l.progress("Restoring channels").await;
        let (map, created, t) = restore_channels(&env, b, &reuse_channels, &role_map).await;
        chan_map = map;
        created_channels = created;
        chan_tally.created = t.created;
        chan_tally.edited = t.edited;
        chan_tally.failed += t.failed;
    }
    if o.channels || want_channel_deletes {
        l.lines.push(format!("✅ {}", chan_tally.line("Channels")));
    }
    if l.cancelled() {
        return;
    }

    // Everything left needs the roles and channels but not each other.
    let wants_members = (o.members || o.dm_invite) && !b.members.is_empty();
    let live = if wants_members {
        l.progress("Reading the member list").await;
        match all_members(env.ctx, env.gid).await {
            Ok(m) => Some(m),
            Err(e) => {
                l.lines.push(format!("⚠️ **Members:** skipped, I couldn't read the member list: {e}"));
                None
            }
        }
    } else {
        None
    };
    l.progress("Restoring settings, messages and members").await;
    let (settings, messages, members) = tokio::join!(
        async { if o.settings { Some(restore_settings(&env, b, &chan_map).await) } else { None } },
        async { if o.messages { Some(replay_messages(&env, b, &chan_map, &created_channels).await) } else { None } },
        async {
            match (&live, o.members) {
                (Some(live), true) => Some(restore_members(&env, b, live, &role_map).await),
                _ => None,
            }
        },
    );
    l.lines.extend(settings);
    l.lines.extend(messages);
    l.lines.extend(members);
    if l.cancelled() {
        return;
    }

    if let (Some(live), true) = (&live, o.dm_invite) {
        l.progress("Inviting members back").await;
        let line = invite_members(&env, b, live, &banned_now, l.i.channel_id).await;
        l.lines.push(line);
    }
}

async fn load(ctx: &Context, i: &CommandInteraction, info: &GuildInfo, b: Backup, o: LoadOptions) {
    let gid = info.id;
    let me = ctx.cache.current_user().id;
    let Some(bot) = fetch_member(ctx, gid, me).await else {
        return respond(ctx, i, Tone::Error, None, "I couldn't check my own permissions here. Try again in a moment.").await;
    };
    let my_perms = ctx.cache.guild(gid).map(|g| g.member_permissions(&bot)).unwrap_or_default();
    let mut need = Permissions::MANAGE_ROLES | Permissions::MANAGE_CHANNELS | Permissions::MANAGE_GUILD;
    if o.bans {
        need |= Permissions::BAN_MEMBERS;
    }
    if o.members {
        need |= Permissions::MANAGE_NICKNAMES;
    }
    if o.emojis {
        need |= Permissions::MANAGE_GUILD_EXPRESSIONS;
    }
    if o.dm_invite {
        need |= Permissions::CREATE_INSTANT_INVITE;
    }
    if o.ban_members {
        need |= Permissions::BAN_MEMBERS;
    }
    if o.messages {
        need |= Permissions::MANAGE_WEBHOOKS;
    }
    if !my_perms.administrator() && !my_perms.contains(need) {
        let missing = need - my_perms;
        return respond(ctx, i, Tone::Error, None, &format!("I'm missing permissions for this load: `{missing}`.")).await;
    }

    let live_channels = match gid.channels(&ctx.http).await {
        Ok(c) => c,
        Err(e) => {
            return respond(ctx, i, Tone::Error, None, &format!("I couldn't read this server's channels ({e}), so I haven't touched anything.")).await;
        }
    };
    let live_roles: Vec<LiveRole> = info
        .roles
        .iter()
        .map(|(id, r)| LiveRole {
            id: id.get(),
            name: r.name.clone(),
            locked: *id == gid.everyone_role() || r.managed || r.position >= info.bot_highest,
        })
        .collect();
    let live_chans: Vec<LiveChannel> = live_channels
        .values()
        .map(|c| LiveChannel { id: c.id.get(), name: c.name.clone(), kind: channel_kind_num(c.kind) })
        .collect();
    let plan = backups::plan(&b, &live_roles, &live_chans, o.delete_roles, o.delete_channels, i.channel_id.get());

    let mut what = Vec::new();
    if o.delete_roles {
        what.push(format!("• **Delete {}** role(s) that aren't in the backup", plan.delete_roles.len()));
    }
    if o.delete_channels {
        what.push(format!("• **Delete {}** channel(s) that aren't in the backup (not this one)", plan.delete_channels.len()));
    }
    if o.roles {
        what.push(format!("• Restore **{}** roles ({} matched by name and edited)", b.roles.len(), plan.reuse_roles.len()));
    }
    if o.channels {
        what.push(format!(
            "• Restore **{}** channels ({} matched by name and edited)",
            b.channels.len(),
            plan.reuse_channels.len()
        ));
    }
    if o.settings {
        what.push("• Overwrite the server name, icon and settings".to_string());
    }
    if o.emojis {
        what.push(format!("• Add missing emojis (**{}** in the backup)", b.emojis.len()));
    }
    if o.bans {
        what.push(format!("• Re-apply **{}** bans", b.bans.len()));
    }
    if o.members {
        what.push(format!("• Give **{}** members their saved roles and nicknames", b.members.len()));
    }
    if o.messages {
        what.push(format!(
            "• Replay **{}** saved messages, skipping any that are still there",
            b.counts.messages
        ));
    }
    if o.ban_members {
        let here = ctx.cache.guild(gid).map(|g| g.member_count).unwrap_or(0);
        what.push(format!(
            "• **Ban every current member** (about {here}), except bots, the server owner, bot owners and you"
        ));
    }
    if o.dm_invite {
        what.push("• **DM an invite** to every saved member who isn't in this server (one a second)".to_string());
    }
    let body = format!(
        "Loading **{}** (`{}`) from <t:{}:f> into **{}**. This will:\n{}\n\nThis can't be undone. Take a `/backup create` first if you might want this server back.",
        b.guild_name,
        b.id,
        b.created_at / 1000,
        info.name,
        what.join("\n")
    );
    let buttons = CreateActionRow::Buttons(vec![
        CreateButton::new("backup_confirm").label("Load it").style(ButtonStyle::Danger),
        CreateButton::new("backup_abort").label("Cancel").style(ButtonStyle::Secondary),
    ]);
    let msg = CreateInteractionResponseMessage::new()
        .embed(theme::card(Tone::Warning, Some("Load this backup?"), body))
        .components(vec![buttons])
        .ephemeral(true);
    if i.create_response(&ctx.http, CreateInteractionResponse::Message(msg)).await.is_err() {
        return;
    }
    let Ok(prompt) = i.get_response(&ctx.http).await else { return };
    let user = i.user.id;
    let click = ComponentInteractionCollector::new(&ctx.shard)
        .timeout(Duration::from_secs(60))
        .filter(move |c| c.message.id == prompt.id && c.user.id == user)
        .next()
        .await;
    let confirmed = match click {
        Some(c) => {
            let _ = c.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
            c.data.custom_id == "backup_confirm"
        }
        None => false,
    };
    if !confirmed {
        let card = theme::card(Tone::Info, None, "Cancelled, nothing was changed.");
        let _ = i.edit_response(&ctx.http, EditInteractionResponse::new().embed(card).components(vec![])).await;
        return;
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let busy = {
        let mut running = loads();
        let busy = running.contains_key(&gid);
        if !busy {
            running.insert(gid, cancel.clone());
        }
        busy
    };
    if busy {
        let card = theme::card(Tone::Error, None, "A backup is already loading here. Wait for it, or `/backup cancel` it.");
        let _ = i.edit_response(&ctx.http, EditInteractionResponse::new().embed(card).components(vec![])).await;
        return;
    }
    println!("💾 [{gid}] {} is loading backup {} ({})", i.user.id, b.id, b.guild_name);
    let mut loader = Loader { ctx, i, guild_id: gid, cancel, lines: Vec::new() };
    run_load(&mut loader, &b, o, plan).await;
    loads().remove(&gid);

    let (tone, title) = if loader.cancelled() {
        loader.lines.push("🛑 Cancelled. Everything above was already applied.".to_string());
        (Tone::Warning, "Backup load cancelled")
    } else {
        (Tone::Success, "Backup loaded")
    };
    let card = theme::card(tone, Some(title), loader.lines.join("\n"));
    // The interaction token lasts 15 minutes, which a big server can outrun.
    if i.edit_response(&ctx.http, EditInteractionResponse::new().embed(card.clone()).components(vec![])).await.is_err() {
        try_dm_embed(&ctx.http, i.user.id, card).await;
    }
}

// ── Interval backups ──────────────────────────────────────────

/// Take every interval backup that's due. Each replaces the one before it.
pub async fn run_due_intervals(ctx: &Context) {
    let now = now_ms();
    for (gid, mut iv) in due_intervals(now) {
        let Some(guild_id) = id_of(&gid).map(GuildId::new) else { continue };
        let Some(guild_owner) = ctx.cache.guild(guild_id).map(|g| g.owner_id) else { continue };
        // A schedule a bot owner set up keeps running whoever owns the server.
        let by_bot_owner = crate::common::permissions::is_owner_str(&iv.owner_id);
        if !by_bot_owner && guild_owner.to_string() != iv.owner_id {
            println!("💾 [{gid}] interval backups stopped: the server changed owner");
            set_interval(&gid, None);
            continue;
        }
        let Some(owner) = id_of(&iv.owner_id).map(UserId::new) else { continue };
        match capture(ctx, guild_id, owner, true).await {
            Ok((b, _)) if backups::save(&b) => {
                if let Some(old) = iv.last_backup.take() {
                    backups::delete(&old, Some(&iv.owner_id));
                }
                iv.last_backup = Some(b.id);
                iv.next_at = now + iv.hours * 3_600_000;
            }
            Ok(_) => iv.next_at = now + 3_600_000,
            Err(e) => {
                eprintln!("⚠️ [{gid}] interval backup failed, retrying in an hour: {e}");
                iv.next_at = now + 3_600_000;
            }
        }
        set_interval(&gid, Some(iv));
    }
}

// ── Command ───────────────────────────────────────────────────

async fn respond(ctx: &Context, i: &CommandInteraction, tone: Tone, title: Option<&str>, text: &str) {
    let msg = CreateInteractionResponseMessage::new().embed(theme::card(tone, title, text)).ephemeral(true);
    let _ = i.create_response(&ctx.http, CreateInteractionResponse::Message(msg)).await;
}

async fn edit(ctx: &Context, i: &CommandInteraction, e: CreateEmbed) {
    let _ = i.edit_response(&ctx.http, EditInteractionResponse::new().embed(e)).await;
}

fn ago(ms: i64) -> String {
    let mins = (now_ms() - ms).max(0) / 60_000;
    match mins {
        0..=59 => format!("{mins}m ago"),
        60..=2879 => format!("{}h ago", mins / 60),
        _ => format!("{}d ago", mins / 1440),
    }
}

fn contents(c: &Counts) -> String {
    format!(
        "{} roles · {} channels · {} emojis · {} bans · {} members · {} messages",
        c.roles, c.channels, c.emojis, c.bans, c.members, c.messages
    )
}

pub async fn handle(ctx: &Context, i: &CommandInteraction, info: &GuildInfo) {
    let bot_owner = crate::common::permissions::is_owner(i.user.id);
    if i.user.id != info.owner_id && !bot_owner {
        return respond(ctx, i, Tone::Denied, None, OWNER_ONLY).await;
    }
    let options = i.data.options();
    let Some(ResolvedOption { name: sub, value: ResolvedValue::SubCommand(args), .. }) = options.first() else { return };
    let str_opt = |n: &str| {
        args.iter().find(|o| o.name == n).and_then(|o| match &o.value {
            ResolvedValue::String(s) => Some(s.trim().to_string()),
            _ => None,
        })
    };
    let bool_opt = |n: &str, default: bool| {
        args.iter()
            .find(|o| o.name == n)
            .and_then(|o| match o.value {
                ResolvedValue::Boolean(b) => Some(b),
                _ => None,
            })
            .unwrap_or(default)
    };
    let int_opt = |n: &str| {
        args.iter().find(|o| o.name == n).and_then(|o| match o.value {
            ResolvedValue::Integer(v) => Some(v),
            _ => None,
        })
    };
    let owner = i.user.id.to_string();

    match *sub {
        "create" => {
            if !bot_owner && backups::manual_count(&owner) >= MAX_PER_USER {
                return respond(ctx, i, Tone::Error, None, &format!("You already have {MAX_PER_USER} backups. Delete one with `/backup delete` first.")).await;
            }
            let defer = CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new().ephemeral(true));
            let _ = i.create_response(&ctx.http, defer).await;
            match capture(ctx, info.id, i.user.id, false).await {
                Ok((b, warnings)) => {
                    if !backups::save(&b) {
                        return edit(ctx, i, theme::card(Tone::Error, None, "I took the backup but couldn't save it to the database. The bot's log has the reason.")).await;
                    }
                    let mut body = format!(
                        "**ID:** `{}`\n{}\n\nLoad it with `/backup load id:{}`, here or in any server you own.",
                        b.id,
                        contents(&b.counts),
                        b.id
                    );
                    for w in warnings {
                        body.push_str(&format!("\n⚠️ {w}"));
                    }
                    edit(ctx, i, theme::card(Tone::Success, Some("Backup created"), body)).await;
                }
                Err(e) => edit(ctx, i, theme::card(Tone::Error, None, format!("The backup failed: {e}"))).await,
            }
        }
        "load" => {
            let id = str_opt("id").unwrap_or_default();
            let Some(b) = backups::get(&id, scope(&owner)) else {
                return respond(ctx, i, Tone::Error, None, "You don't have a backup with that ID. `/backup list` shows yours.").await;
            };
            let o = LoadOptions {
                settings: bool_opt("settings", true),
                roles: bool_opt("roles", true),
                channels: bool_opt("channels", true),
                delete_roles: bool_opt("delete_roles", true),
                delete_channels: bool_opt("delete_channels", true),
                emojis: bool_opt("emojis", true),
                bans: bool_opt("bans", false),
                members: bool_opt("members", false),
                dm_invite: bool_opt("dm_invite", false),
                messages: bool_opt("messages", false),
                ban_members: bool_opt("ban_members", false),
            };
            load(ctx, i, info, b, o).await;
        }
        "list" => {
            let all = backups::list(scope(&owner));
            if all.is_empty() {
                return respond(ctx, i, Tone::Info, Some("Your backups"), "You haven't made any backups yet. `/backup create` makes one.").await;
            }
            // A bot owner sees everyone's, so keep it inside an embed.
            const SHOWN: usize = 20;
            let mut body = all
                .iter()
                .take(SHOWN)
                .map(|m| {
                    format!(
                        "{}`{}` **{}** · <t:{}:R>{}\n{}",
                        if m.interval { "🕒 " } else { "" },
                        m.id,
                        m.guild_name,
                        m.created_at / 1000,
                        if m.owner_id == owner { String::new() } else { format!(" · by <@{}>", m.owner_id) },
                        contents(&m.counts)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            if all.len() > SHOWN {
                body.push_str(&format!("\n\n…and {} more. Type in the `id` box to search them.", all.len() - SHOWN));
            }
            respond(ctx, i, Tone::Info, Some("Your backups"), &format!("{body}\n\n🕒 = interval backup")).await;
        }
        "info" => {
            let id = str_opt("id").unwrap_or_default();
            let Some(b) = backups::get(&id, scope(&owner)) else {
                return respond(ctx, i, Tone::Error, None, "You don't have a backup with that ID.").await;
            };
            let roles: Vec<String> = b.roles.iter().rev().take(15).map(|r| r.name.clone()).collect();
            let channels: Vec<String> = b
                .channels
                .iter()
                .filter(|c| c.kind != 4)
                .take(15)
                .map(|c| format!("#{}", c.name))
                .collect();
            let e = theme::card(
                Tone::Info,
                Some("Backup info"),
                format!(
                    "**ID:** `{}`{}\n**Server:** {} (`{}`)\n**Taken:** <t:{}:f>\n{}",
                    b.id,
                    if b.interval { " 🕒" } else { "" },
                    b.guild_name,
                    b.guild_id,
                    b.created_at / 1000,
                    contents(&b.counts)
                ),
            )
            .field("Top roles", if roles.is_empty() { "-".into() } else { roles.join(", ") }, false)
            .field("Channels", if channels.is_empty() { "-".into() } else { channels.join(", ") }, false);
            let msg = CreateInteractionResponseMessage::new().embed(e).ephemeral(true);
            let _ = i.create_response(&ctx.http, CreateInteractionResponse::Message(msg)).await;
        }
        "delete" => {
            let id = str_opt("id").unwrap_or_default();
            if backups::delete(&id, scope(&owner)) {
                respond(ctx, i, Tone::Success, None, &format!("Deleted backup `{id}`.")).await;
            } else {
                respond(ctx, i, Tone::Error, None, "You don't have a backup with that ID.").await;
            }
        }
        "interval" => {
            let gid = info.id.to_string();
            let current = backups::interval(&gid);
            let Some(enabled) = args.iter().find(|o| o.name == "enabled").and_then(|o| match o.value {
                ResolvedValue::Boolean(b) => Some(b),
                _ => None,
            }) else {
                let text = match current {
                    Some(iv) => format!(
                        "On: every **{}h**, next <t:{}:R>. Each one replaces the last.",
                        iv.hours,
                        iv.next_at / 1000
                    ),
                    None => "Off. `/backup interval enabled:true hours:24` turns it on.".to_string(),
                };
                return respond(ctx, i, Tone::Info, Some("Interval backups"), &text).await;
            };
            if !enabled {
                backups::set_interval(&gid, None);
                return respond(ctx, i, Tone::Success, None, "Interval backups are off. The last one is kept in `/backup list`.").await;
            }
            let hours = int_opt("hours").unwrap_or(24).clamp(INTERVAL_HOURS.0, INTERVAL_HOURS.1);
            let iv = Interval {
                owner_id: owner,
                hours,
                // First one on the next timer tick.
                next_at: now_ms(),
                last_backup: current.and_then(|iv| iv.last_backup),
            };
            let saved = backups::set_interval(&gid, Some(iv));
            let mut text = format!("I'll back this server up every **{hours}h**, starting within the next few minutes. Each one replaces the last.");
            if !saved {
                text.push_str("\n\n❌ It didn't save to the database, so it will stop at the next restart.");
            }
            respond(ctx, i, Tone::Success, Some("Interval backups on"), &text).await;
        }
        "cancel" => {
            let flag = loads().get(&info.id).cloned();
            match flag {
                Some(f) => {
                    f.store(true, Ordering::Relaxed);
                    respond(ctx, i, Tone::Success, None, "Stopping the load after the current step.").await;
                }
                None => respond(ctx, i, Tone::Info, None, "No backup is loading here.").await,
            }
        }
        _ => {}
    }
}

/// Suggest the backups this user can reach for the `id` option.
pub async fn autocomplete(ctx: &Context, i: &CommandInteraction) {
    let typed = i.data.autocomplete().map(|o| o.value.to_lowercase()).unwrap_or_default();
    let user = i.user.id.to_string();
    let mut resp = CreateAutocompleteResponse::new();
    for m in backups::list(scope(&user))
        .into_iter()
        .filter(|m| m.id.contains(&typed) || m.guild_name.to_lowercase().contains(&typed))
        .take(25)
    {
        let label: String =
            format!("{}{} · {} · {}", if m.interval { "🕒 " } else { "" }, m.guild_name, ago(m.created_at), m.id).chars().take(100).collect();
        resp = resp.add_string_choice(label, m.id);
    }
    let _ = i.create_response(&ctx.http, CreateInteractionResponse::Autocomplete(resp)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(content: &str) -> BMessage {
        BMessage { author: "a".into(), avatar: String::new(), content: content.into(), embeds: vec![], attachments: vec![], at: 1_700_000_000, pinned: false }
    }

    #[test]
    fn replayed_text_keeps_the_date_and_fits_discord() {
        let short = replay_text(&msg("hello"));
        assert_eq!(short, "hello\n-# <t:1700000000:f>");
        let long = replay_text(&msg(&"x".repeat(5000)));
        assert!(long.chars().count() <= 2000);
        assert!(long.ends_with("<t:1700000000:f>"));
    }

    #[test]
    fn a_replayed_copy_matches_its_original() {
        let mut original = msg("hello there");
        original.attachments.push(BAttachment { name: "a.png".into(), url: "https://x/a.png".into() });
        let copy = BMessage { content: replay_text(&original), at: 1_800_000_000, ..msg("") };
        assert_eq!(message_key(&copy), message_key(&original));
        let other = BMessage { at: original.at + 1, ..original.clone() };
        assert_ne!(message_key(&other), message_key(&original));
        assert_ne!(message_key(&msg("different")), message_key(&original));
    }

    #[tokio::test]
    async fn par_runs_everything_at_once_up_to_its_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (now, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let started = std::time::Instant::now();
        let out = par(0..40u32, 8, |n| {
            let (now, peak) = (&now, &peak);
            async move {
                let running = now.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(running, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                now.fetch_sub(1, Ordering::SeqCst);
                n * 2
            }
        })
        .await;
        let mut sorted = out.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..40).map(|n| n * 2).collect::<Vec<_>>(), "every item is processed exactly once");
        assert!(peak.load(Ordering::SeqCst) > 1, "items must overlap");
        assert!(peak.load(Ordering::SeqCst) <= 8, "never more than the limit");
        // 40 x 20ms in series is 800ms; 8 at a time is about 100ms.
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn failure_notes_only_appear_when_something_failed() {
        assert_eq!(failed_note(0, "failed"), "");
        assert_eq!(failed_note(3, "failed"), ", **3 failed**");
    }

    #[test]
    fn ban_members_spares_bots_owners_and_the_caller() {
        let (owner, me, someone) = (UserId::new(1), UserId::new(2), UserId::new(3));
        assert!(spared_from_ban(UserId::new(9), true, Some(owner), me), "bots stay");
        assert!(spared_from_ban(owner, false, Some(owner), me));
        assert!(spared_from_ban(me, false, Some(owner), me));
        let bot_owner = crate::common::config::BOT_OWNER_IDS.iter().next().and_then(|s| s.parse().ok()).map(UserId::new).unwrap();
        assert!(spared_from_ban(bot_owner, false, Some(owner), me));
        assert!(!spared_from_ban(someone, false, Some(owner), me));
    }

    #[test]
    fn webhook_names_follow_discords_rules() {
        assert_eq!(webhook_name("Alice"), "Alice");
        assert_eq!(webhook_name("DiscordMod"), "Former member");
        assert_eq!(webhook_name("  "), "Former member");
        assert_eq!(webhook_name(&"n".repeat(100)).len(), 80);
    }
}
