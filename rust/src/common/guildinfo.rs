//! Owned snapshots of cached guild data.
//!
//! serenity's cache hands back a guard that cannot be held across an `.await`,
//! so anything that needs role positions/permissions while also making API
//! calls copies what it needs out of the cache first via [`GuildInfo`].

use serenity::client::Context;
use serenity::model::guild::Member;
use serenity::model::id::{ChannelId, GuildId, RoleId, UserId};
use serenity::model::Permissions;
use std::collections::HashMap;

#[derive(Clone)]
pub struct RoleInfo {
    pub name: String,
    pub position: i64,
    pub managed: bool,
    pub permissions: Permissions,
    pub colour: u32,
    pub hoist: bool,
    pub mentionable: bool,
}

#[derive(Clone)]
pub struct GuildInfo {
    pub id: GuildId,
    pub name: String,
    pub owner_id: UserId,
    pub roles: HashMap<RoleId, RoleInfo>,
    /// Highest role position held by the bot itself, or 0 if uncached.
    pub bot_highest: i64,
}

impl GuildInfo {
    pub fn from_cache(ctx: &Context, guild_id: GuildId) -> Option<Self> {
        let bot_id = ctx.cache.current_user().id;
        let guild = ctx.cache.guild(guild_id)?;
        let roles: HashMap<RoleId, RoleInfo> = guild
            .roles
            .iter()
            .map(|(id, r)| {
                (
                    *id,
                    RoleInfo {
                        name: r.name.to_string(),
                        position: r.position as i64,
                        managed: r.managed,
                        permissions: r.permissions,
                        colour: r.colour.0,
                        hoist: r.hoist,
                        mentionable: r.mentionable,
                    },
                )
            })
            .collect();
        let bot_highest = guild
            .members
            .get(&bot_id)
            .map(|m| highest_of(&roles, &m.roles))
            .unwrap_or(0);
        Some(Self {
            id: guild_id,
            name: guild.name.to_string(),
            owner_id: guild.owner_id,
            roles,
            bot_highest,
        })
    }

    pub fn highest_position(&self, role_ids: &[RoleId]) -> i64 {
        highest_of(&self.roles, role_ids)
    }

    pub fn member_highest(&self, member: &Member) -> i64 {
        self.highest_position(&member.roles)
    }

    /// discord.js's `role.editable`: below the bot's top role and not an
    /// integration-managed role.
    pub fn role_editable(&self, role_id: RoleId) -> bool {
        self.roles
            .get(&role_id)
            .map(|r| !r.managed && r.position < self.bot_highest)
            .unwrap_or(false)
    }

    pub fn role_name(&self, role_id: RoleId) -> String {
        self.roles.get(&role_id).map(|r| r.name.clone()).unwrap_or_else(|| role_id.to_string())
    }

    /// Every role the member holds that carries any dangerous permission and
    /// that the bot is actually able to remove.
    pub fn dangerous_editable_roles(&self, member_roles: &[RoleId]) -> Vec<RoleId> {
        let mask = *super::config::DANGER_PERMS_MASK;
        member_roles
            .iter()
            .filter(|rid| {
                self.roles
                    .get(rid)
                    .map(|r| r.permissions.intersects(mask) && !r.managed && r.position < self.bot_highest)
                    .unwrap_or(false)
            })
            .copied()
            .collect()
    }
}

fn highest_of(roles: &HashMap<RoleId, RoleInfo>, role_ids: &[RoleId]) -> i64 {
    role_ids.iter().filter_map(|rid| roles.get(rid)).map(|r| r.position).max().unwrap_or(0)
}

/// An owned [`Member`], from cache when possible and over HTTP otherwise.
/// Cloning out of the cache immediately keeps the guard from crossing an
/// `.await`.
pub async fn fetch_member(ctx: &Context, guild_id: GuildId, user_id: UserId) -> Option<Member> {
    let cached = ctx.cache.guild(guild_id).and_then(|g| g.members.get(&user_id).cloned());
    if cached.is_some() {
        return cached;
    }
    guild_id.member(&ctx.http, user_id).await.ok()
}

/// Whether `channel_id` is one of `guild_id`'s channels or threads, per the
/// cache.
///
/// Every channel the bot posts to because a setting names it is checked with
/// this first. A stored id can be stale (the channel was deleted) or foreign
/// (seeded, imported or hand-edited from another server), and without the
/// check the second case sends one server's logs into another.
pub fn channel_in_guild(ctx: &Context, guild_id: GuildId, channel_id: ChannelId) -> bool {
    ctx.cache
        .guild(guild_id)
        .map(|g| g.channels.contains_key(&channel_id) || g.threads.iter().any(|t| t.id == channel_id))
        .unwrap_or(false)
}

/// A stored channel id, if it parses and belongs to this guild.
pub fn guild_channel(ctx: &Context, guild_id: GuildId, raw: &str) -> Option<ChannelId> {
    let id = raw.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new)?;
    channel_in_guild(ctx, guild_id, id).then_some(id)
}

/// Every member of the guild, paging through the API 1000 at a time.
///
/// `GuildId::members` returns one page and says nothing about the rest, which
/// on a large server meant snapshots and the failsafe only ever saw the first
/// thousand members. An error is returned rather than a partial list, because
/// the callers go on to add and remove roles based on who is missing.
pub async fn all_members(ctx: &Context, guild_id: GuildId) -> serenity::Result<Vec<Member>> {
    use futures::StreamExt;
    let mut stream = Box::pin(guild_id.members_iter(&ctx.http));
    let mut out = Vec::new();
    while let Some(m) = stream.next().await {
        out.push(m?);
    }
    Ok(out)
}
