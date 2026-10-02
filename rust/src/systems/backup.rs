//! `!backup`: Xenon-style server backups. Server owner only.
//!
//! A backup belongs to the user who took it and can be loaded into any server
//! that user owns, so it doubles as a way to clone a server. Every part a load
//! is asked to restore is rebuilt from scratch: what's there is deleted and
//! the backup's version created, rather than matched up and edited.

use once_cell::sync::Lazy;
use serenity::builder::{
    CreateActionRow, CreateAllowedMentions, CreateAttachment, CreateButton, CreateChannel, CreateEmbed,
    CreateInteractionResponse, CreateInvite, CreateMessage, CreateWebhook, EditGuild, EditMember, EditMessage, EditRole,
    ExecuteWebhook, GetMessages,
};
use serenity::client::Context;
use serenity::collector::ComponentInteractionCollector;
use serenity::http::{LightMethod, Request, Route};
use serenity::model::application::ButtonStyle;
use serenity::model::channel::{Message, MessageType, PermissionOverwrite, PermissionOverwriteType};
use serenity::model::guild::{AfkTimeout, DefaultMessageNotificationLevel, ExplicitContentFilter, VerificationLevel};
use serenity::model::id::{ChannelId, GuildId, MessageId, RoleId, UserId};
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

const OWNER_ONLY: &str = "Backups are for the server owner only.";

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
    deleted: usize,
    failed: usize,
}

impl Tally {
    fn line(&self, what: &str) -> String {
        let mut s = format!("**{what}:** {} deleted, {} created", self.deleted, self.created);
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
    /// The `!backup load` message.
    cmd: &'a Message,
    /// The bot's reply, edited as the load goes.
    status: MessageId,
    guild_id: GuildId,
    cancel: Arc<AtomicBool>,
    lines: Vec<String>,
    report_channel_gone: bool,
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
        body.push_str(&format!("⏳ {step}…\n\n_`!backup cancel` stops it; what's done stays done._"));
        let card = theme::card(Tone::Info, Some("Loading backup"), body);
        let _ = self
            .cmd
            .channel_id
            .edit_message(&self.ctx.http, self.status, EditMessage::new().embed(card).components(vec![]))
            .await;
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
    // Whatever is there goes first, so the backup's set comes back as-is.
    let existing: Vec<serenity::model::id::EmojiId> = env
        .ctx
        .cache
        .guild(env.gid)
        .map(|g| g.emojis.values().filter(|e| !e.managed).map(|e| e.id).collect())
        .unwrap_or_default();
    let removed = par(existing, 6, |id| async move {
        !env.cancelled() && env.gid.delete_emoji(&env.ctx.http, id).await.is_ok()
    })
    .await;
    let removed = removed.iter().filter(|ok| **ok).count();
    let done = par(&b.emojis, 6, |e| async move {
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
    format!("✅ **Emojis:** {removed} removed, {made} added{}", failed_note(done.len() - made, "failed"))
}

/// One bulk request to set positions, instead of one request per item.
async fn patch_positions(env: &Env<'_>, route: Route<'_>, body: serde_json::Value) {
    let Ok(bytes) = serde_json::to_vec(&body) else { return };
    let req = Request::new(route, LightMethod::Patch).body(Some(bytes));
    if let Err(e) = env.ctx.http.fire::<serde_json::Value>(req).await {
        eprintln!("⚠️ [{}] backup load couldn't set positions: {e}", env.gid);
    }
}

async fn restore_roles(env: &Env<'_>, b: &Backup) -> (HashMap<String, RoleId>, Tally) {
    let results = par(b.roles.iter(), FAST, |r| async move {
        if env.cancelled() {
            return (&r.id, None);
        }
        let builder = EditRole::new()
            .name(r.name.clone())
            .colour(r.color as u64)
            .hoist(r.hoist)
            .mentionable(r.mentionable)
            .permissions(perms(&r.permissions))
            .audit_log_reason(&env.reason);
        (&r.id, env.gid.create_role(&env.ctx.http, builder).await.ok().map(|role| role.id))
    })
    .await;

    let mut map = HashMap::new();
    let mut t = Tally::default();
    for (id, res) in results {
        match res {
            Some(live) => {
                t.created += 1;
                map.insert(id.clone(), live);
            }
            None => t.failed += 1,
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

/// Create one channel; `None` if Discord refused.
async fn restore_channel(
    env: &Env<'_>,
    c: &BChannel,
    parent: Option<ChannelId>,
    overwrites: Vec<PermissionOverwrite>,
) -> Option<ChannelId> {
    if env.cancelled() {
        return None;
    }
    let mut e = CreateChannel::new(c.name.clone()).kind(kind_from_num(c.kind)).permissions(overwrites);
    if let Some(p) = parent {
        e = e.category(p);
    }
    if matches!(c.kind, 0 | 5 | 15) {
        e = e.nsfw(c.nsfw).rate_limit_per_user(c.rate_limit);
        if let Some(topic) = &c.topic {
            e = e.topic(topic.clone());
        }
    }
    if matches!(c.kind, 2 | 13) {
        if let Some(br) = c.bitrate {
            e = e.bitrate(br);
        }
        if let Some(ul) = c.user_limit {
            e = e.user_limit(ul);
        }
    }
    env.gid.create_channel(&env.ctx.http, e.audit_log_reason(&env.reason)).await.ok().map(|ch| ch.id)
}

/// Categories first, all at once, then everything inside them, all at once.
async fn restore_channels(
    env: &Env<'_>,
    b: &Backup,
    role_map: &HashMap<String, RoleId>,
) -> (HashMap<String, ChannelId>, Tally) {
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

    let mut map = HashMap::new();
    let mut t = Tally::default();
    let (cats, rest): (Vec<&BChannel>, Vec<&BChannel>) = b.channels.iter().partition(|c| c.kind == 4);
    for group in [cats, rest] {
        let results = par(group, FAST, |c| {
            let parent = c.parent_id.as_ref().and_then(|p| map.get(p).copied());
            let overwrites = remap(&c.overwrites);
            async move { (&c.id, restore_channel(env, c, parent, overwrites).await) }
        })
        .await;
        for (id, res) in results {
            match res {
                Some(live) => {
                    t.created += 1;
                    map.insert(id.clone(), live);
                }
                None => t.failed += 1,
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
    (map, t)
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

/// Replay every one of a channel's saved messages in order, through its own
/// webhook, whatever the channel already holds.
async fn replay_channel(env: &Env<'_>, c: &BChannel, channel: ChannelId) -> (usize, usize) {
    let (ctx, mut sent, mut failed) = (env.ctx, 0usize, 0usize);
    let hook = match channel.create_webhook(&ctx.http, CreateWebhook::new("Backup restore")).await {
        Ok(h) => h,
        Err(_) => return (0, c.messages.len()),
    };
    for m in &c.messages {
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
    (sent, failed)
}

async fn replay_messages(
    env: &Env<'_>,
    b: &Backup,
    chan_map: &HashMap<String, ChannelId>,
) -> String {
    let targets: Vec<(&BChannel, ChannelId)> = b
        .channels
        .iter()
        .filter(|c| !c.messages.is_empty())
        .filter_map(|c| chan_map.get(&c.id).map(|id| (c, *id)))
        .collect();
    let results = par(&targets, REPLAYS, |(c, channel)| replay_channel(env, c, *channel)).await;
    let (sent, failed) = results.iter().fold((0, 0), |a, r| (a.0 + r.0, a.1 + r.1));
    format!(
        "✅ **Messages:** {sent} replayed across {} channel(s){}",
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
    let invoker = l.cmd.author.id;

    // Deleting, banning and emojis don't depend on each other or on anything
    // restored later, so they all run together.
    l.progress("Clearing out the old server").await;
    let want_role_deletes = !plan.delete_roles.is_empty();
    let want_channel_deletes = !plan.delete_channels.is_empty();
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
    let mut role_map = HashMap::new();
    if o.roles {
        l.progress("Restoring roles").await;
        let (map, t) = restore_roles(&env, b).await;
        role_map = map;
        role_tally.created = t.created;
        role_tally.failed += t.failed;
    }
    if o.roles || want_role_deletes {
        l.lines.push(format!("✅ {}", role_tally.line("Roles")));
    }
    if l.cancelled() {
        return;
    }

    let mut chan_tally = channel_deletes.unwrap_or_default();
    let mut chan_map = HashMap::new();
    if o.channels {
        l.progress("Restoring channels").await;
        let (map, t) = restore_channels(&env, b, &role_map).await;
        chan_map = map;
        chan_tally.created = t.created;
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
        async { if o.messages { Some(replay_messages(&env, b, &chan_map).await) } else { None } },
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
        // The channel this was run from is deleted along with the rest, so
        // the invite points at a restored one.
        let channel = if o.channels || o.delete_channels { invite_channel(b, &chan_map) } else { Some(l.cmd.channel_id) };
        match channel {
            Some(channel) => {
                let line = invite_members(&env, b, live, &banned_now, channel).await;
                l.lines.push(line);
            }
            None => l.lines.push("⚠️ **Invites:** not sent, no restored text channel to invite into".to_string()),
        }
    }

    // Deleting channels keeps nothing that was there before, including the
    // channel this load was started from. It goes last so progress stays
    // visible until the end; the report then comes by DM.
    if (o.channels || o.delete_channels) && !l.cancelled() {
        l.report_channel_gone = l.cmd.channel_id.delete(&env.ctx.http).await.is_ok();
    }
}

/// Where invites point: the restored system channel, else the first restored
/// text channel.
fn invite_channel(b: &Backup, chan_map: &HashMap<String, ChannelId>) -> Option<ChannelId> {
    b.settings
        .system_channel
        .as_ref()
        .and_then(|c| chan_map.get(c))
        .or_else(|| b.channels.iter().filter(|c| c.kind == 0).find_map(|c| chan_map.get(&c.id)))
        .copied()
}

/// `!wipe` (name configurable): ban every member and delete every role and
/// channel, with nothing recreated - a blank server. Nothing is backed up
/// first, so it asks for a button confirmation before doing anything.
async fn wipe(ctx: &Context, msg: &Message, info: &GuildInfo) {
    let gid = info.id;
    let me = ctx.cache.current_user().id;
    let Some(bot) = fetch_member(ctx, gid, me).await else {
        return respond(ctx, msg, Tone::Error, None, "I couldn't check my own permissions here. Try again in a moment.").await;
    };
    let my_perms = ctx.cache.guild(gid).map(|g| g.member_permissions(&bot)).unwrap_or_default();
    let bot_top = info.highest_position(&bot.roles).max(info.bot_highest);
    let need = Permissions::MANAGE_ROLES | Permissions::MANAGE_CHANNELS | Permissions::BAN_MEMBERS;
    if !my_perms.administrator() && !my_perms.contains(need) {
        let missing = need - my_perms;
        return respond(ctx, msg, Tone::Error, None, &format!("I'm missing permissions to wipe this server: `{missing}`.")).await;
    }

    let live_channels = match gid.channels(&ctx.http).await {
        Ok(c) => c,
        Err(e) => return respond(ctx, msg, Tone::Error, None, &format!("I couldn't read this server's channels ({e}), so I haven't touched anything.")).await,
    };
    let live_roles: Vec<LiveRole> = info
        .roles
        .iter()
        .map(|(id, r)| LiveRole { id: id.get(), locked: *id == gid.everyone_role() || r.managed || r.position >= bot_top })
        .collect();
    let live_chans: Vec<LiveChannel> =
        live_channels.values().map(|c| LiveChannel { id: c.id.get(), kind: channel_kind_num(c.kind) }).collect();
    let plan = backups::plan(&live_roles, &live_chans, true, true, msg.channel_id.get());
    let here = ctx.cache.guild(gid).map(|g| g.member_count).unwrap_or(0);

    let body = format!(
        "**This wipes __{}__ to a blank server. There is no backup and no undo.**\n\n• **Ban every current member** (about {here}), except bots, the server owner, bot owners and you\n• **Delete all {} roles** I can manage\n• **Delete all {} channels**, including this one (last)\n\nClick **Wipe the server** within 60s to go ahead.",
        info.name,
        plan.delete_roles.len(),
        plan.delete_channels.len() + 1
    );
    let buttons = CreateActionRow::Buttons(vec![
        CreateButton::new("backup_wipe_confirm").label("Wipe the server").style(ButtonStyle::Danger),
        CreateButton::new("backup_wipe_abort").label("Cancel").style(ButtonStyle::Secondary),
    ]);
    let prompt = CreateMessage::new()
        .embed(theme::card(Tone::Error, Some("Wipe this server?"), body))
        .components(vec![buttons])
        .reference_message(msg);
    let Ok(prompt) = msg.channel_id.send_message(&ctx.http, prompt).await else { return };
    let prompt = prompt.id;

    let author = msg.author.id;
    let channel = msg.channel_id;
    let click = ComponentInteractionCollector::new(&ctx.shard)
        .timeout(Duration::from_secs(60))
        .filter(move |c| c.message.id == prompt && c.user.id == author)
        .next()
        .await;
    let confirmed = match click {
        Some(c) => {
            let _ = c.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
            c.data.custom_id == "backup_wipe_confirm"
        }
        None => false,
    };
    if !confirmed {
        let card = theme::card(Tone::Info, None, "Wipe cancelled, nothing was changed.");
        let _ = channel.edit_message(&ctx.http, prompt, EditMessage::new().embed(card).components(vec![])).await;
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
        return respond(ctx, msg, Tone::Error, None, "A backup is already running here. Wait for it, or `!backup cancel` it.").await;
    }
    println!("💥 [{gid}] {author} is wiping the server");
    let mut loader = Loader { ctx, cmd: msg, status: prompt, guild_id: gid, cancel, lines: Vec::new(), report_channel_gone: false };
    run_wipe(&mut loader, plan).await;
    loads().remove(&gid);

    let (tone, title) = if loader.cancelled() {
        loader.lines.push("🛑 Cancelled. Everything above was already applied.".to_string());
        (Tone::Warning, "Wipe cancelled")
    } else {
        (Tone::Success, "Server wiped")
    };
    let card = theme::card(tone, Some(title), loader.lines.join("\n"));
    if loader.report_channel_gone
        || channel.edit_message(&ctx.http, prompt, EditMessage::new().embed(card.clone()).components(vec![])).await.is_err()
    {
        try_dm_embed(&ctx.http, author, card).await;
    }
}

/// Ban everyone and delete every role and channel. The deletes and the ban
/// run together; the reporting channel goes last so progress stays visible.
async fn run_wipe(l: &mut Loader<'_>, plan: Plan) {
    let cancel = l.cancel.clone();
    let env = Env { ctx: l.ctx, gid: l.guild_id, cancel: &cancel, reason: "Server wipe".to_string() };
    let invoker = l.cmd.author.id;

    l.progress("Banning members and clearing out roles and channels").await;
    let (role_deletes, channel_deletes, bans) = tokio::join!(
        async { if !plan.delete_roles.is_empty() { Some(delete_roles(&env, &plan.delete_roles).await) } else { None } },
        async { if !plan.delete_channels.is_empty() { Some(delete_channels(&env, &plan.delete_channels).await) } else { None } },
        ban_current(&env, invoker),
    );
    l.lines.push(bans.1);
    if let Some(t) = role_deletes {
        l.lines.push(format!("✅ {}", t.line("Roles")));
    }
    if let Some(t) = channel_deletes {
        l.lines.push(format!("✅ {}", t.line("Channels")));
    }
    if l.cancelled() {
        return;
    }
    // The channel this was run from is the one place still standing; it goes
    // last so the progress above stays readable, and the report then DMs.
    l.report_channel_gone = l.cmd.channel_id.delete(&env.ctx.http).await.is_ok();
}

async fn load(ctx: &Context, msg: &Message, info: &GuildInfo, b: Backup, o: LoadOptions) {
    let gid = info.id;
    let me = ctx.cache.current_user().id;
    let Some(bot) = fetch_member(ctx, gid, me).await else {
        return respond(ctx, msg, Tone::Error, None, "I couldn't check my own permissions here. Try again in a moment.").await;
    };
    let my_perms = ctx.cache.guild(gid).map(|g| g.member_permissions(&bot)).unwrap_or_default();
    // From the member just fetched rather than the cache, which can be
    // missing the bot and put it at the bottom.
    let bot_top = info.highest_position(&bot.roles).max(info.bot_highest);
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
        return respond(ctx, msg, Tone::Error, None, &format!("I'm missing permissions for this load: `{missing}`.")).await;
    }

    let live_channels = match gid.channels(&ctx.http).await {
        Ok(c) => c,
        Err(e) => {
            return respond(ctx, msg, Tone::Error, None, &format!("I couldn't read this server's channels ({e}), so I haven't touched anything.")).await;
        }
    };
    let live_roles: Vec<LiveRole> = info
        .roles
        .iter()
        .map(|(id, r)| LiveRole {
            id: id.get(),
            locked: *id == gid.everyone_role() || r.managed || r.position >= bot_top,
        })
        .collect();
    let live_chans: Vec<LiveChannel> = live_channels
        .values()
        .map(|c| LiveChannel { id: c.id.get(), kind: channel_kind_num(c.kind) })
        .collect();
    // Roles and channels are never matched up or kept: switched on, every
    // existing one is deleted and the backup's are created fresh.
    let wipe_roles = o.roles || o.delete_roles;
    let wipe_channels = o.channels || o.delete_channels;
    let plan = backups::plan(&live_roles, &live_chans, wipe_roles, wipe_channels, msg.channel_id.get());

    let mut what = Vec::new();
    if wipe_roles {
        // Say why any are left, so "0" never reads as a bug.
        let above = info
            .roles
            .iter()
            .filter(|(id, r)| **id != gid.everyone_role() && !r.managed && r.position >= bot_top)
            .count();
        let managed = info.roles.values().filter(|r| r.managed).count();
        let mut line = format!("• **Delete all {} roles** I can manage", plan.delete_roles.len());
        if above + managed > 0 {
            line.push_str(&format!(
                " (leaving {above} at or above my top role, position {bot_top}, and {managed} owned by bots or integrations)"
            ));
        }
        what.push(line);
    }
    if o.roles {
        what.push(format!("• Create the backup's **{}** roles", b.roles.len()));
    }
    if wipe_channels {
        what.push(format!("• **Delete all {} channels**, including this one (last)", plan.delete_channels.len() + 1));
    }
    if o.channels {
        what.push(format!("• Create the backup's **{}** channels", b.channels.len()));
    }
    if o.settings {
        what.push("• Overwrite the server name, icon and settings".to_string());
    }
    if o.emojis {
        what.push(format!("• Replace every emoji with the backup's **{}**", b.emojis.len()));
    }
    if o.bans {
        what.push(format!("• Re-apply **{}** bans", b.bans.len()));
    }
    if o.members {
        what.push(format!("• Give **{}** members their saved roles and nicknames", b.members.len()));
    }
    if o.messages {
        what.push(format!(
            "• Replay all **{}** saved messages",
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
        "Loading **{}** (`{}`) from <t:{}:f> into **{}**. This will:\n{}\n\nThis can't be undone. Take a `!backup create` first if you might want this server back.",
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
    let prompt = CreateMessage::new()
        .embed(theme::card(Tone::Warning, Some("Load this backup?"), body))
        .components(vec![buttons])
        .reference_message(msg);
    let Ok(prompt) = msg.channel_id.send_message(&ctx.http, prompt).await else { return };
    let status = prompt.id;
    let user = msg.author.id;
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
        let _ = msg.channel_id.edit_message(&ctx.http, status, EditMessage::new().embed(card).components(vec![])).await;
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
        let card = theme::card(Tone::Error, None, "A backup is already loading here. Wait for it, or `!backup cancel` it.");
        let _ = msg.channel_id.edit_message(&ctx.http, status, EditMessage::new().embed(card).components(vec![])).await;
        return;
    }
    println!("💾 [{gid}] {} is loading backup {} ({})", msg.author.id, b.id, b.guild_name);
    let mut loader = Loader { ctx, cmd: msg, status, guild_id: gid, cancel, lines: Vec::new(), report_channel_gone: false };
    run_load(&mut loader, &b, o, plan).await;
    loads().remove(&gid);

    let (tone, title) = if loader.cancelled() {
        loader.lines.push("🛑 Cancelled. Everything above was already applied.".to_string());
        (Tone::Warning, "Backup load cancelled")
    } else {
        (Tone::Success, "Backup loaded")
    };
    let card = theme::card(tone, Some(title), loader.lines.join("\n"));
    // Deleting channels deletes the one the reply lives in.
    if loader.report_channel_gone
        || msg
            .channel_id
            .edit_message(&ctx.http, status, EditMessage::new().embed(card.clone()).components(vec![]))
            .await
            .is_err()
    {
        try_dm_embed(&ctx.http, msg.author.id, card).await;
    }
}

// ── Interval backups ──────────────────────────────────────────

/// Take every interval backup that's due. Each replaces the one before it.
pub async fn run_due_intervals(ctx: &Context) {
    let now = now_ms();
    for (gid, mut iv) in due_intervals(now) {
        let Some(guild_id) = id_of(&gid).map(GuildId::new) else { continue };
        let Some(guild_owner) = ctx.cache.guild(guild_id).map(|g| g.owner_id) else { continue };
        if guild_owner.to_string() != iv.owner_id {
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

pub const PREFIX: &str = "!backup";

/// The word that triggers a full server wipe, used as its own `!` command.
/// Read once from `backup_wipe_command.txt` at the repo root (default `wipe`),
/// so it can be renamed - or set to something only you know - without touching
/// the code. If the file holds `wipe`, the command is `!wipe`. Bot owner only.
pub static WIPE_COMMAND: Lazy<String> = Lazy::new(|| {
    std::fs::read_to_string(crate::common::config::root_file("backup_wipe_command.txt"))
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "wipe".to_string())
});

/// The full standalone trigger for a wipe, e.g. `!wipe`.
pub static WIPE_TRIGGER: Lazy<String> = Lazy::new(|| format!("!{}", *WIPE_COMMAND));

/// Whether `content`'s first word is the standalone wipe command (e.g. `!wipe`).
pub fn is_wipe_trigger(content: &str) -> bool {
    content.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case(&WIPE_TRIGGER))
}

const USAGE: &str = "`!backup create` · take a backup of this server
`!backup list` · your backups
`!backup info <id>` · what's in one
`!backup delete <id>`
`!backup load <id> [options]` · e.g. `!backup load abc123 roles channels messages dm_invite`
`!backup interval` · show the schedule · `!backup interval on 24` · `!backup interval off`
`!backup cancel` · stop a load

**Load options** (`name`, `name=true` or `name=false`):
on by default: `delete_roles` `delete_channels` `ban_members`
off by default: `roles` `channels` `settings` `emojis` `bans` `members` `messages` `dm_invite`
With no options a load deletes every role and channel and bans everyone; add `roles channels` to recreate the backup's.";

async fn respond(ctx: &Context, msg: &Message, tone: Tone, title: Option<&str>, text: &str) {
    let _ = reply(ctx, msg, theme::card(tone, title, text)).await;
}

async fn reply(ctx: &Context, msg: &Message, card: CreateEmbed) -> Option<Message> {
    msg.channel_id.send_message(&ctx.http, CreateMessage::new().embed(card).reference_message(msg)).await.ok()
}

fn contents(c: &Counts) -> String {
    format!(
        "{} roles · {} channels · {} emojis · {} bans · {} members · {} messages",
        c.roles, c.channels, c.emojis, c.bans, c.members, c.messages
    )
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// `!backup load <id> [name | name=bool | name:bool ...]`.
fn parse_load(args: &[&str]) -> Result<(String, LoadOptions), String> {
    let Some((id, flags)) = args.split_first() else {
        return Err("Which backup? `!backup load <id>` - `!backup list` shows your IDs.".into());
    };
    // On its own, a load wipes the server: every role, every channel, and
    // everyone in it. Restoring anything is opt-in.
    let mut o = LoadOptions {
        settings: false,
        roles: false,
        channels: false,
        delete_roles: true,
        delete_channels: true,
        emojis: false,
        bans: false,
        members: false,
        dm_invite: false,
        messages: false,
        ban_members: true,
    };
    for flag in flags {
        let (name, value) = match flag.split_once(['=', ':']) {
            Some((n, v)) => (n, parse_bool(v).ok_or_else(|| format!("`{flag}`: use true or false."))?),
            None => (*flag, true),
        };
        let slot = match name.to_ascii_lowercase().as_str() {
            "settings" => &mut o.settings,
            "roles" => &mut o.roles,
            "channels" => &mut o.channels,
            "delete_roles" => &mut o.delete_roles,
            "delete_channels" => &mut o.delete_channels,
            "emojis" => &mut o.emojis,
            "bans" => &mut o.bans,
            "members" => &mut o.members,
            "dm_invite" => &mut o.dm_invite,
            "messages" => &mut o.messages,
            "ban_members" => &mut o.ban_members,
            _ => return Err(format!("I don't know the option `{name}`.\n\n{USAGE}")),
        };
        *slot = value;
    }
    Ok((id.to_string(), o))
}

/// Handle the standalone wipe command (e.g. `!wipe`). Bot owner only; for
/// anyone else it stays silent so the command's name isn't given away.
pub async fn handle_wipe(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    if !crate::common::permissions::is_owner(msg.author.id) {
        return;
    }
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else {
        return respond(ctx, msg, Tone::Error, None, "I'm still loading this server's details. Give it a few seconds and try again.").await;
    };
    wipe(ctx, msg, &info).await;
}

/// Handle a `!backup …` message. Server owner and bot owners only.
pub async fn handle_message(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    let words: Vec<&str> = msg.content.split_whitespace().collect();
    if !words.first().is_some_and(|w| w.eq_ignore_ascii_case(PREFIX)) {
        return;
    }
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else {
        return respond(ctx, msg, Tone::Error, None, "I'm still loading this server's details. Give it a few seconds and try again.").await;
    };
    let sub = words.get(1).map(|w| w.to_ascii_lowercase()).unwrap_or_default();
    let args = words.get(2..).unwrap_or_default();
    let owner = msg.author.id.to_string();

    if msg.author.id != info.owner_id {
        return respond(ctx, msg, Tone::Denied, None, OWNER_ONLY).await;
    }

    match sub.as_str() {
        "create" => {
            if backups::manual_count(&owner) >= MAX_PER_USER {
                return respond(ctx, msg, Tone::Error, None, &format!("You already have {MAX_PER_USER} backups. Delete one with `!backup delete <id>` first.")).await;
            }
            let Some(working) = reply(ctx, msg, theme::card(Tone::Info, Some("Backing up"), "⏳ Taking a backup of this server…")).await else {
                return;
            };
            let card = match capture(ctx, info.id, msg.author.id, false).await {
                Ok((b, warnings)) if backups::save(&b) => {
                    let mut body = format!(
                        "**ID:** `{}`\n{}\n\nLoad it with `!backup load {}`, here or in any server you own.",
                        b.id,
                        contents(&b.counts),
                        b.id
                    );
                    for w in warnings {
                        body.push_str(&format!("\n⚠️ {w}"));
                    }
                    theme::card(Tone::Success, Some("Backup created"), body)
                }
                Ok(_) => theme::card(Tone::Error, None, "I took the backup but couldn't save it to the database. The bot's log has the reason."),
                Err(e) => theme::card(Tone::Error, None, format!("The backup failed: {e}")),
            };
            let _ = msg.channel_id.edit_message(&ctx.http, working.id, EditMessage::new().embed(card)).await;
        }
        "load" => {
            let (id, o) = match parse_load(args) {
                Ok(v) => v,
                Err(e) => return respond(ctx, msg, Tone::Error, None, &e).await,
            };
            let Some(b) = backups::get(&id, Some(&owner)) else {
                return respond(ctx, msg, Tone::Error, None, "You don't have a backup with that ID. `!backup list` shows yours.").await;
            };
            load(ctx, msg, &info, b, o).await;
        }
        "list" => {
            let all = backups::list(Some(&owner));
            if all.is_empty() {
                return respond(ctx, msg, Tone::Info, Some("Your backups"), "You haven't made any backups yet. `!backup create` makes one.").await;
            }
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
                body.push_str(&format!("\n\n…and {} more.", all.len() - SHOWN));
            }
            respond(ctx, msg, Tone::Info, Some("Your backups"), &format!("{body}\n\n🕒 = interval backup")).await;
        }
        "info" => {
            let id = args.first().copied().unwrap_or_default();
            let Some(b) = backups::get(id, Some(&owner)) else {
                return respond(ctx, msg, Tone::Error, None, "You don't have a backup with that ID. `!backup info <id>`").await;
            };
            let roles: Vec<String> = b.roles.iter().rev().take(15).map(|r| r.name.clone()).collect();
            let channels: Vec<String> =
                b.channels.iter().filter(|c| c.kind != 4).take(15).map(|c| format!("#{}", c.name)).collect();
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
            let _ = reply(ctx, msg, e).await;
        }
        "delete" => {
            let id = args.first().copied().unwrap_or_default();
            if backups::delete(id, Some(&owner)) {
                respond(ctx, msg, Tone::Success, None, &format!("Deleted backup `{id}`.")).await;
            } else {
                respond(ctx, msg, Tone::Error, None, "You don't have a backup with that ID. `!backup delete <id>`").await;
            }
        }
        "interval" => {
            let gid = info.id.to_string();
            let current = backups::interval(&gid);
            let Some(enabled) = args.first().and_then(|a| parse_bool(a)) else {
                let text = match current {
                    Some(iv) => format!("On: every **{}h**, next <t:{}:R>. Each one replaces the last.", iv.hours, iv.next_at / 1000),
                    None => "Off. `!backup interval on 24` turns it on (every 6 to 168 hours).".to_string(),
                };
                return respond(ctx, msg, Tone::Info, Some("Interval backups"), &text).await;
            };
            if !enabled {
                backups::set_interval(&gid, None);
                return respond(ctx, msg, Tone::Success, None, "Interval backups are off. The last one is kept in `!backup list`.").await;
            }
            let hours = args.get(1).and_then(|h| h.parse::<i64>().ok()).unwrap_or(24).clamp(INTERVAL_HOURS.0, INTERVAL_HOURS.1);
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
            respond(ctx, msg, Tone::Success, Some("Interval backups on"), &text).await;
        }
        "cancel" => {
            let flag = loads().get(&info.id).cloned();
            match flag {
                Some(f) => {
                    f.store(true, Ordering::Relaxed);
                    respond(ctx, msg, Tone::Success, None, "Stopping the load after the current step.").await;
                }
                None => respond(ctx, msg, Tone::Info, None, "No backup is loading here.").await,
            }
        }
        _ => respond(ctx, msg, Tone::Info, Some("Backups"), USAGE).await,
    }
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
    fn load_options_parse_from_the_message() {
        // Bare: delete roles and channels, ban members, nothing else.
        let (id, o) = parse_load(&["abc123"]).unwrap();
        assert_eq!(id, "abc123");
        assert!(o.delete_roles && o.delete_channels && o.ban_members);
        assert!(!o.roles && !o.channels && !o.settings && !o.emojis && !o.bans);
        assert!(!o.members && !o.messages && !o.dm_invite);

        let (_, o) = parse_load(&["abc", "roles", "channels=true", "ban_members=false", "messages:yes", "dm_invite=on"]).unwrap();
        assert!(o.roles && o.channels && o.messages && o.dm_invite);
        assert!(!o.ban_members && !o.emojis && o.delete_roles);

        assert!(parse_load(&[]).is_err(), "an id is required");
        assert!(parse_load(&["abc", "nukes"]).is_err(), "unknown options are refused, not ignored");
        assert!(parse_load(&["abc", "roles=maybe"]).is_err());
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
