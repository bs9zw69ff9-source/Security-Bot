//! Anti-Ping: punish mentions of protected users/roles.
//!
//! Everything is judged against the settings of the guild the message was sent
//! in: its protected users and roles, its exemptions, its punishment.

use once_cell::sync::Lazy;
use serenity::builder::CreateMessage;
use serenity::client::Context;
use serenity::model::channel::Message;
use serenity::model::id::{GuildId, RoleId, UserId};
use serenity::model::Timestamp;
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use crate::common::config::now_ms;
use crate::common::embeds::{colors, embed, render_anti_ping_response, sec_log};
use crate::common::guildinfo::{fetch_member, GuildInfo};
use crate::common::permissions::{is_mod, is_whitelisted};
use crate::state::anti_ping::{ap, AntiPing};
use super::mute::mute_user;

/// (guild, user) -> when they were last punished, for the cooldown.
static LAST_PUNISHED: Lazy<Mutex<HashMap<(GuildId, UserId), i64>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn cooldown_lock() -> std::sync::MutexGuard<'static, HashMap<(GuildId, UserId), i64>> {
    LAST_PUNISHED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Claim a punishment slot; false while the user is still cooling down.
fn claim_cooldown(guild_id: GuildId, user_id: UserId, now: i64, cooldown_sec: i64) -> bool {
    if cooldown_sec <= 0 {
        return true;
    }
    let mut map = cooldown_lock();
    match map.get(&(guild_id, user_id)) {
        Some(at) if now - at < cooldown_sec * 1000 => false,
        _ => {
            map.insert((guild_id, user_id), now);
            true
        }
    }
}

/// One mentioned user, with the roles they hold in this guild when known.
struct Mention {
    id: UserId,
    bot: bool,
    roles: Vec<RoleId>,
}

/// Which protected targets a message hit, as mention strings in a stable
/// order. Pure, so the rules can be tested without Discord.
fn protected_hits(
    cfg: &AntiPing,
    author: UserId,
    replied_to: Option<UserId>,
    mentions: &[Mention],
    mention_roles: &[RoleId],
) -> BTreeSet<String> {
    let mut hits = BTreeSet::new();
    for m in mentions {
        if m.id == author || m.bot {
            continue;
        }
        if cfg.ignore_replies && replied_to == Some(m.id) {
            continue;
        }
        let protected_user = cfg.protected_users.contains(&m.id.to_string());
        let protected_role = m.roles.iter().any(|r| cfg.protected_roles.contains(&r.to_string()));
        if protected_user || protected_role {
            hits.insert(format!("<@{}>", m.id));
        }
    }
    for rid in mention_roles {
        if cfg.protected_roles.contains(&rid.to_string()) {
            hits.insert(format!("<@&{rid}>"));
        }
    }
    hits
}

pub async fn check_anti_ping(ctx: &Context, msg: &Message, info: &GuildInfo) {
    let a = ap(&info.id.to_string());
    if !a.enabled || (a.protected_users.is_empty() && a.protected_roles.is_empty()) {
        return;
    }
    if msg.mentions.is_empty() && msg.mention_roles.is_empty() {
        return;
    }
    if a.exempt_channels.contains(&msg.channel_id.to_string()) {
        return;
    }
    let Some(member) = fetch_member(ctx, info.id, msg.author.id).await else { return };
    if member.user.id == info.owner_id {
        return;
    }
    if is_mod(&member, info.owner_id) || is_whitelisted(&member, info.owner_id) {
        return;
    }
    if member.roles.iter().any(|r| a.exempt_roles.contains(&r.to_string())) {
        return;
    }

    // Role lookups only matter when this guild protects roles. The gateway
    // sends each mention's roles with the message; the member fetch is the
    // fallback for when it doesn't.
    let mut mentions = Vec::with_capacity(msg.mentions.len());
    for user in &msg.mentions {
        let roles = if a.protected_roles.is_empty() {
            Vec::new()
        } else if let Some(pm) = &user.member {
            pm.roles.clone()
        } else {
            fetch_member(ctx, info.id, user.id).await.map(|m| m.roles).unwrap_or_default()
        };
        mentions.push(Mention { id: user.id, bot: user.bot, roles });
    }
    let replied_to = msg.referenced_message.as_ref().map(|m| m.author.id);
    let hits = protected_hits(&a, msg.author.id, replied_to, &mentions, &msg.mention_roles);
    if hits.is_empty() {
        return;
    }

    let targets = hits.into_iter().collect::<Vec<_>>().join(", ");
    let reason = format!("Anti-ping: mentioned protected {targets}");
    if a.delete_message {
        let _ = msg.delete(&ctx.http).await;
    }
    if !claim_cooldown(info.id, member.user.id, now_ms(), a.cooldown_sec) {
        return;
    }

    let action_text = match a.action.as_str() {
        "mute" => {
            mute_user(ctx, info, &member, a.timeout_min, &reason).await;
            format!("muted for {} min", a.timeout_min)
        }
        "timeout" => {
            let until = Timestamp::from_millis(now_ms() + a.timeout_min * 60_000).unwrap_or_else(|_| Timestamp::now());
            let mut m = member.clone();
            if let Err(e) = m.disable_communication_until_datetime(&ctx.http, until).await {
                eprintln!("⚠️ [{}] anti-ping couldn't time out {}: {e}", info.id, member.user.id);
            }
            format!("timed out for {} min", a.timeout_min)
        }
        "warn" => {
            let me = ctx.cache.current_user().id.to_string();
            let total =
                crate::state::warnings::add_warning(&info.id.to_string(), &member.user.id.to_string(), &reason, &me);
            format!("warned (warning #{total})")
        }
        _ => "logged only".to_string(),
    };

    if a.notify_channel {
        let text = render_anti_ping_response(&a.response_template, &member.user.id.to_string(), &targets, &action_text);
        if let Ok(sent) = msg
            .channel_id
            .send_message(&ctx.http, CreateMessage::new().embed(embed(colors::WARN, text, Some("Anti-Ping"))))
            .await
        {
            // Self-clean the public notice after 8s, same as the JS version.
            let http = ctx.http.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                let _ = sent.delete(&http).await;
            });
        }
    }

    sec_log(
        ctx,
        info.id,
        "📡 Anti-Ping Triggered",
        &format!(
            "<@{}> pinged {targets} in <#{}>, so they were **{action_text}**.",
            member.user.id, msg.channel_id
        ),
        colors::WARN,
    )
    .await;
}

/// Drop cooldowns that have long run out.
pub fn sweep() {
    let now = now_ms();
    cooldown_lock().retain(|_, at| now - *at < 3_600_000);
}

pub fn forget_guild(guild_id: GuildId) {
    cooldown_lock().retain(|(g, _), _| *g != guild_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(users: &[u64], roles: &[u64]) -> AntiPing {
        let mut c = AntiPing::defaults();
        c.protected_users = users.iter().map(|u| u.to_string()).collect();
        c.protected_roles = roles.iter().map(|r| r.to_string()).collect();
        c.ignore_replies = true;
        c
    }

    fn mention(id: u64, roles: &[u64]) -> Mention {
        Mention { id: UserId::new(id), bot: false, roles: roles.iter().map(|r| RoleId::new(*r)).collect() }
    }

    #[test]
    fn mentions_are_judged_by_the_guild_they_happen_in() {
        // Guild A protects user 10; guild B protects nobody.
        let (a, b) = (cfg(&[10], &[]), cfg(&[], &[]));
        let m = [mention(10, &[])];
        assert_eq!(protected_hits(&a, UserId::new(1), None, &m, &[]).len(), 1);
        assert!(protected_hits(&b, UserId::new(1), None, &m, &[]).is_empty());
    }

    #[test]
    fn a_protected_role_matches_by_id_not_name() {
        // Both guilds have a role called "Staff", with different ids.
        let guild_a = cfg(&[], &[500]);
        let staff_in_b = [mention(20, &[600])];
        assert!(protected_hits(&guild_a, UserId::new(1), None, &staff_in_b, &[]).is_empty());
        let staff_in_a = [mention(20, &[500])];
        assert_eq!(protected_hits(&guild_a, UserId::new(1), None, &staff_in_a, &[]).len(), 1);
        assert_eq!(protected_hits(&guild_a, UserId::new(1), None, &[], &[RoleId::new(500)]).len(), 1);
    }

    #[test]
    fn self_pings_bots_and_replies_are_ignored() {
        let c = cfg(&[10, 11], &[]);
        assert!(protected_hits(&c, UserId::new(10), None, &[mention(10, &[])], &[]).is_empty());
        let bot = Mention { id: UserId::new(11), bot: true, roles: vec![] };
        assert!(protected_hits(&c, UserId::new(1), None, &[bot], &[]).is_empty());
        assert!(protected_hits(&c, UserId::new(1), Some(UserId::new(10)), &[mention(10, &[])], &[]).is_empty());
    }

    #[test]
    fn cooldowns_are_per_guild() {
        let user = UserId::new(31);
        let (a, b) = (GuildId::new(7_000_001), GuildId::new(7_000_002));
        assert!(claim_cooldown(a, user, 0, 10));
        assert!(!claim_cooldown(a, user, 5_000, 10));
        assert!(claim_cooldown(b, user, 5_000, 10), "guild A's cooldown must not hold in guild B");
        assert!(claim_cooldown(a, user, 11_000, 10));
    }
}
