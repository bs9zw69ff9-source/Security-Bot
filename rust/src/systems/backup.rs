//! `/backup`: Xenon-style server backups. Server owner only.
//!
//! A backup belongs to the user who took it and can be loaded into any server
//! that user owns, so it doubles as a way to clone a server. Loading edits
//! same-named roles and channels in place, creates what's missing and, if asked,
//! deletes what isn't in the backup.

use once_cell::sync::Lazy;
use serenity::builder::{
    CreateActionRow, CreateAttachment, CreateAutocompleteResponse, CreateButton, CreateChannel, CreateEmbed,
    CreateInteractionResponse, CreateInteractionResponseMessage, EditChannel, EditGuild, EditInteractionResponse,
    EditMember, EditRole,
};
use serenity::client::Context;
use serenity::collector::ComponentInteractionCollector;
use serenity::model::application::{ButtonStyle, CommandInteraction, ResolvedOption, ResolvedValue};
use serenity::model::channel::{PermissionOverwrite, PermissionOverwriteType};
use serenity::model::guild::{AfkTimeout, DefaultMessageNotificationLevel, ExplicitContentFilter, VerificationLevel};
use serenity::model::id::{ChannelId, GuildId, RoleId, UserId};
use serenity::model::Permissions;
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
        })
        .collect();
    chans.sort_by_key(|c| c.position);

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
            .filter(|m| m.nick.is_some() || !m.roles.is_empty())
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

async fn run_load(l: &mut Loader<'_>, b: &Backup, o: LoadOptions, plan: Plan) {
    let (ctx, gid) = (l.ctx, l.guild_id);
    let reason = format!("Backup {} loaded by the server owner", b.id);

    // Roles.
    let mut role_map: HashMap<String, RoleId> =
        plan.reuse_roles.iter().map(|(k, v)| (k.clone(), RoleId::new(*v))).collect();
    let mut t = Tally::default();
    if o.delete_roles && !plan.delete_roles.is_empty() {
        l.progress("Deleting roles that aren't in the backup").await;
        for id in &plan.delete_roles {
            if l.cancelled() {
                return;
            }
            match gid.delete_role(&ctx.http, RoleId::new(*id)).await {
                Ok(()) => t.deleted += 1,
                Err(_) => t.failed += 1,
            }
        }
    }
    if o.roles {
        l.progress("Restoring roles").await;
        // Highest first: a new role lands at the bottom, so this leaves the
        // created ones in the backup's order.
        for r in b.roles.iter().rev() {
            if l.cancelled() {
                return;
            }
            let builder = EditRole::new()
                .name(r.name.clone())
                .colour(r.color as u64)
                .hoist(r.hoist)
                .mentionable(r.mentionable)
                .permissions(perms(&r.permissions))
                .audit_log_reason(&reason);
            match role_map.get(&r.id) {
                Some(live) => match gid.edit_role(&ctx.http, *live, builder).await {
                    Ok(_) => t.edited += 1,
                    Err(_) => t.failed += 1,
                },
                None => match gid.create_role(&ctx.http, builder).await {
                    Ok(role) => {
                        t.created += 1;
                        role_map.insert(r.id.clone(), role.id);
                    }
                    Err(_) => t.failed += 1,
                },
            }
        }
    }
    if o.roles || o.delete_roles {
        l.lines.push(format!("✅ {}", t.line("Roles")));
    }

    let everyone_src = b.guild_id.clone();
    let remap = |ows: &[BOverwrite]| -> Vec<PermissionOverwrite> {
        ows.iter()
            .filter_map(|o| {
                let (allow, deny) = (perms(&o.allow), perms(&o.deny));
                let kind = if o.kind == 1 {
                    PermissionOverwriteType::Member(UserId::new(id_of(&o.id)?))
                } else if o.id == everyone_src {
                    PermissionOverwriteType::Role(gid.everyone_role())
                } else {
                    PermissionOverwriteType::Role(*role_map.get(&o.id)?)
                };
                Some(PermissionOverwrite { allow, deny, kind })
            })
            .collect()
    };

    // Channels.
    let mut chan_map: HashMap<String, ChannelId> =
        plan.reuse_channels.iter().map(|(k, v)| (k.clone(), ChannelId::new(*v))).collect();
    let mut t = Tally::default();
    if o.delete_channels && !plan.delete_channels.is_empty() {
        l.progress("Deleting channels that aren't in the backup").await;
        for id in &plan.delete_channels {
            if l.cancelled() {
                return;
            }
            match ChannelId::new(*id).delete(&ctx.http).await {
                Ok(_) => t.deleted += 1,
                Err(_) => t.failed += 1,
            }
        }
    }
    if o.channels {
        l.progress("Restoring channels").await;
        let (cats, rest): (Vec<&BChannel>, Vec<&BChannel>) = b.channels.iter().partition(|c| c.kind == 4);
        for c in cats.into_iter().chain(rest) {
            if l.cancelled() {
                return;
            }
            let kind = kind_from_num(c.kind);
            let voice = matches!(c.kind, 2 | 13);
            let texty = matches!(c.kind, 0 | 5 | 15);
            let parent = c.parent_id.as_ref().and_then(|p| chan_map.get(p).copied());
            let position = c.position.clamp(0, u16::MAX as i64) as u16;
            let overwrites = remap(&c.overwrites);
            match chan_map.get(&c.id).copied() {
                Some(live) => {
                    let mut e = EditChannel::new().name(c.name.clone()).position(position).permissions(overwrites);
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
                    match live.edit(&ctx.http, e.audit_log_reason(&reason)).await {
                        Ok(_) => t.edited += 1,
                        Err(_) => t.failed += 1,
                    }
                }
                None => {
                    let mut e =
                        CreateChannel::new(c.name.clone()).kind(kind).position(position).permissions(overwrites);
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
                    match gid.create_channel(&ctx.http, e.audit_log_reason(&reason)).await {
                        Ok(ch) => {
                            t.created += 1;
                            chan_map.insert(c.id.clone(), ch.id);
                        }
                        Err(_) => t.failed += 1,
                    }
                }
            }
        }
    }
    if o.channels || o.delete_channels {
        l.lines.push(format!("✅ {}", t.line("Channels")));
    }

    if o.settings {
        if l.cancelled() {
            return;
        }
        l.progress("Restoring server settings").await;
        let s = &b.settings;
        let mut e = EditGuild::new()
            .name(s.name.clone())
            .verification_level(VerificationLevel::from(s.verification_level))
            .default_message_notifications(Some(DefaultMessageNotificationLevel::from(s.default_notifications)))
            .explicit_content_filter(Some(ExplicitContentFilter::from(s.explicit_content_filter)))
            .afk_timeout(afk_timeout(s.afk_timeout))
            .afk_channel(s.afk_channel.as_ref().and_then(|c| chan_map.get(c).copied()))
            .system_channel_id(s.system_channel.as_ref().and_then(|c| chan_map.get(c).copied()))
            .audit_log_reason(&reason);
        let icon = match &s.icon_url {
            Some(url) => CreateAttachment::url(&ctx.http, url).await.ok(),
            None => None,
        };
        if icon.is_some() {
            e = e.icon(icon.as_ref());
        }
        let mut notes = Vec::new();
        if s.icon_url.is_some() && icon.is_none() {
            notes.push("icon couldn't be downloaded");
        }
        if gid.edit(&ctx.http, e).await.is_err() {
            notes.push("some settings were refused");
        }
        let everyone = EditRole::new().permissions(perms(&s.everyone_permissions));
        if gid.edit_role(&ctx.http, gid.everyone_role(), everyone).await.is_err() {
            notes.push("@everyone permissions weren't changed");
        }
        l.lines.push(if notes.is_empty() {
            "✅ **Settings:** name, icon, verification, notifications, AFK, system channel, @everyone".to_string()
        } else {
            format!("⚠️ **Settings:** restored, but {}", notes.join(", "))
        });
    }

    if o.emojis && !b.emojis.is_empty() {
        l.progress("Restoring emojis").await;
        let existing: HashSet<String> =
            ctx.cache.guild(gid).map(|g| g.emojis.values().map(|e| e.name.clone()).collect()).unwrap_or_default();
        let (mut made, mut failed) = (0, 0);
        for e in b.emojis.iter().filter(|e| !existing.contains(&e.name)) {
            if l.cancelled() {
                return;
            }
            let ok = match CreateAttachment::url(&ctx.http, &e.url).await {
                Ok(img) => gid.create_emoji(&ctx.http, &e.name, &img.to_base64()).await.is_ok(),
                Err(_) => false,
            };
            if ok {
                made += 1;
            } else {
                failed += 1;
            }
        }
        l.lines.push(format!("✅ **Emojis:** {made} added{}", if failed > 0 { format!(", **{failed} failed**") } else { String::new() }));
    }

    if o.bans && !b.bans.is_empty() {
        l.progress("Restoring bans").await;
        let (mut done, mut failed) = (0, 0);
        for ban in &b.bans {
            if l.cancelled() {
                return;
            }
            let Some(uid) = id_of(&ban.user_id) else { continue };
            let why: String = ban.reason.as_deref().unwrap_or("Restored from a backup").chars().take(400).collect();
            match gid.ban_with_reason(&ctx.http, UserId::new(uid), 0, why).await {
                Ok(()) => done += 1,
                Err(_) => failed += 1,
            }
        }
        l.lines.push(format!("✅ **Bans:** {done} restored{}", if failed > 0 { format!(", **{failed} failed**") } else { String::new() }));
    }

    if o.members && !b.members.is_empty() {
        l.progress("Restoring members' roles and nicknames").await;
        let live = match all_members(ctx, gid).await {
            Ok(m) => m,
            Err(e) => {
                l.lines.push(format!("⚠️ **Members:** skipped, I couldn't read the member list: {e}"));
                return;
            }
        };
        let owner = ctx.cache.guild(gid).map(|g| g.owner_id);
        let wanted: HashMap<&str, &BMember> = b.members.iter().map(|m| (m.user_id.as_str(), m)).collect();
        let (mut updated, mut failed) = (0, 0);
        for m in &live {
            if l.cancelled() {
                return;
            }
            let Some(saved) = wanted.get(m.user.id.to_string().as_str()) else { continue };
            let missing: Vec<RoleId> =
                saved.roles.iter().filter_map(|r| role_map.get(r).copied()).filter(|r| !m.roles.contains(r)).collect();
            let nick = saved.nick.clone().filter(|n| m.nick.as_ref() != Some(n) && Some(m.user.id) != owner);
            if missing.is_empty() && nick.is_none() {
                continue;
            }
            let mut roles = m.roles.clone();
            roles.extend(missing);
            let mut e = EditMember::new().roles(roles).audit_log_reason(&reason);
            if let Some(n) = nick {
                e = e.nickname(n);
            }
            match gid.edit_member(&ctx.http, m.user.id, e).await {
                Ok(_) => updated += 1,
                Err(_) => failed += 1,
            }
        }
        l.lines.push(format!(
            "✅ **Members:** {updated} given their roles and nicknames back{}",
            if failed > 0 { format!(", **{failed} failed**") } else { String::new() }
        ));
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
        let Some(owner) = ctx.cache.guild(guild_id).map(|g| g.owner_id) else { continue };
        if owner.to_string() != iv.owner_id {
            println!("💾 [{gid}] interval backups stopped: the server changed owner");
            set_interval(&gid, None);
            continue;
        }
        match capture(ctx, guild_id, owner, true).await {
            Ok((b, _)) if backups::save(&b) => {
                if let Some(old) = iv.last_backup.take() {
                    backups::delete(&old, &iv.owner_id);
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
        "{} roles · {} channels · {} emojis · {} bans · {} members",
        c.roles, c.channels, c.emojis, c.bans, c.members
    )
}

pub async fn handle(ctx: &Context, i: &CommandInteraction, info: &GuildInfo) {
    if i.user.id != info.owner_id {
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
            if backups::manual_count(&owner) >= MAX_PER_USER {
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
            let Some(b) = backups::get(&id, &owner) else {
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
            };
            load(ctx, i, info, b, o).await;
        }
        "list" => {
            let all = backups::list(&owner);
            if all.is_empty() {
                return respond(ctx, i, Tone::Info, Some("Your backups"), "You haven't made any backups yet. `/backup create` makes one.").await;
            }
            let body = all
                .iter()
                .map(|m| {
                    format!(
                        "{}`{}` **{}** · <t:{}:R>\n{}",
                        if m.interval { "🕒 " } else { "" },
                        m.id,
                        m.guild_name,
                        m.created_at / 1000,
                        contents(&m.counts)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            respond(ctx, i, Tone::Info, Some("Your backups"), &format!("{body}\n\n🕒 = interval backup")).await;
        }
        "info" => {
            let id = str_opt("id").unwrap_or_default();
            let Some(b) = backups::get(&id, &owner) else {
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
            if backups::delete(&id, &owner) {
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

/// Suggest the user's own backups for the `id` option.
pub async fn autocomplete(ctx: &Context, i: &CommandInteraction) {
    let typed = i.data.autocomplete().map(|o| o.value.to_lowercase()).unwrap_or_default();
    let mut resp = CreateAutocompleteResponse::new();
    for m in backups::list(&i.user.id.to_string())
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
