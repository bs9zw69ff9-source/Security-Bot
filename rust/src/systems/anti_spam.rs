//! Anti-Spam: mass mentions, scam/grabber links, invite links, duplicate
//! floods, and raw message-frequency floods.
//!
//! Trackers are deliberately in-memory only: their windows are seconds, so a
//! restart naturally (and safely) interrupting a fast burst is fine.

use once_cell::sync::Lazy;
use serenity::client::Context;
use serenity::model::channel::Message;
use serenity::model::id::{GuildId, UserId};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{now_ms, INVITE_RE, SCAM_RE};
use crate::common::embeds::{alert_owner, colors, sec_log};
use crate::common::guildinfo::{fetch_member, GuildInfo};
use crate::common::permissions::{is_mod, is_whitelisted};
use crate::state::guild_settings;
use super::mute::mute_user;

/// Trackers are keyed by guild *and* user: the same person spamming in two
/// servers is two separate floods, judged by two separate configs.
type Key = (GuildId, UserId);

/// recent message timestamps
static SPAM_TRACKER: Lazy<Mutex<HashMap<Key, Vec<i64>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
pub struct Dupe {
    pub content: String,
    pub count: usize,
    pub ts: i64,
}
/// last message + repeat count
static DUPE_TRACKER: Lazy<Mutex<HashMap<Key, Dupe>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn spam_lock() -> std::sync::MutexGuard<'static, HashMap<Key, Vec<i64>>> {
    SPAM_TRACKER.lock().unwrap_or_else(|e| e.into_inner())
}
fn dupe_lock() -> std::sync::MutexGuard<'static, HashMap<Key, Dupe>> {
    DUPE_TRACKER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Count one more copy of `content`; true when the repeat limit is reached.
fn record_duplicate(key: Key, content: &str, now: i64, window_ms: i64, limit: usize) -> bool {
    let mut map = dupe_lock();
    match map.get_mut(&key) {
        Some(d) if d.content == content && now - d.ts < window_ms * 3 => {
            d.count += 1;
            d.ts = now;
            if d.count >= limit {
                map.insert(key, Dupe { content: String::new(), count: 0, ts: now });
                true
            } else {
                false
            }
        }
        _ => {
            map.insert(key, Dupe { content: content.to_string(), count: 1, ts: now });
            false
        }
    }
}

/// Count one more message; true when the flood threshold is reached.
fn record_message(key: Key, now: i64, window_ms: i64, threshold: usize) -> bool {
    let mut map = spam_lock();
    let arr = map.entry(key).or_default();
    arr.retain(|t| now - *t < window_ms);
    arr.push(now);
    if arr.len() >= threshold {
        arr.clear();
        true
    } else {
        false
    }
}

/// Returns true when the message was handled as spam (so anti-ping is skipped).
pub async fn check_spam(ctx: &Context, msg: &Message, info: &GuildInfo) -> bool {
    let cfg = guild_settings::spam(&info.id.to_string());
    if !cfg.enabled {
        return false;
    }
    let Some(member) = fetch_member(ctx, info.id, msg.author.id).await else { return false };
    // Turn spam.exempt_staff off to stress-test on your own staff account.
    if cfg.exempt_staff && (is_mod(&member, info.owner_id) || is_whitelisted(&member, info.owner_id)) {
        return false;
    }
    let uid = msg.author.id;
    let key = (info.id, uid);
    let now = now_ms();
    let channel = msg.channel_id;

    // Mass-mention in a single message (@everyone / @here counts as mass)
    let mention_count = msg.mentions.len()
        + msg.mention_roles.len()
        + if msg.mention_everyone { cfg.mention_limit } else { 0 };
    if mention_count >= cfg.mention_limit {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, cfg.mute_min, &format!("Anti-spam: mass mention ({mention_count})")).await;
        sec_log(
            ctx,
            info.id,
            "Anti-Spam",
            &format!("Muted <@{uid}> for mass-mentioning ({mention_count}) in <#{channel}>."),
            colors::WARN,
        )
        .await;
        return true;
    }

    // Scam / phishing / IP-grabber links
    if cfg.block_scams && SCAM_RE.is_match(&msg.content) {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, cfg.mute_min, "Anti-spam: scam/grabber link").await;
        alert_owner(
            ctx,
            info.id,
            &format!(
                "Heads up - <@{uid}> dropped what looks like a **scam or grabber link** in <#{channel}>. I've deleted it and muted them."
            ),
            colors::DANGER,
            "Scam Link Blocked",
        )
        .await;
        return true;
    }

    // Invite-link spam
    if cfg.block_invites && INVITE_RE.is_match(&msg.content) && !is_mod(&member, info.owner_id) {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, cfg.mute_min, "Anti-spam: posted invite link").await;
        sec_log(
            ctx,
            info.id,
            "Anti-Spam",
            &format!("Muted <@{uid}> for posting an invite link in <#{channel}>."),
            colors::WARN,
        )
        .await;
        return true;
    }

    // Duplicate-message flood
    let dupe_tripped = record_duplicate(key, &msg.content, now, cfg.window_ms, cfg.duplicate_limit);
    if dupe_tripped {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, cfg.mute_min, "Anti-spam: duplicate flood").await;
        sec_log(
            ctx,
            info.id,
            "Anti-Spam",
            &format!("Muted <@{uid}> for flooding the same message over and over in <#{channel}>."),
            colors::WARN,
        )
        .await;
        return true;
    }

    // Frequency flood
    let flood_tripped = record_message(key, now, cfg.window_ms, cfg.message_threshold);
    if flood_tripped {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, cfg.mute_min, "Anti-spam: message flood").await;
        sec_log(
            ctx,
            info.id,
            "Anti-Spam",
            &format!("Muted <@{uid}> for flooding <#{channel}> with messages."),
            colors::WARN,
        )
        .await;
        return true;
    }

    false
}

/// Drop tracker entries that have aged well past their guild's window.
pub fn sweep() {
    let now = now_ms();
    let mut windows: HashMap<GuildId, i64> = HashMap::new();
    let mut window = |g: GuildId| *windows.entry(g).or_insert_with(|| guild_settings::spam(&g.to_string()).window_ms);
    spam_lock().retain(|(g, _), arr| arr.last().map(|t| now - *t <= window(*g) * 5).unwrap_or(false));
    dupe_lock().retain(|(g, _), d| now - d.ts <= window(*g) * 5);
}

/// Forget a guild's trackers (the bot left it).
pub fn forget_guild(guild_id: GuildId) {
    spam_lock().retain(|(g, _), _| *g != guild_id);
    dupe_lock().retain(|(g, _), _| *g != guild_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_user_floods_each_guild_separately() {
        let user = UserId::new(77);
        let (a, b) = ((GuildId::new(8_000_001), user), (GuildId::new(8_000_002), user));
        assert!(!record_message(a, 0, 10_000, 3));
        assert!(!record_message(a, 1, 10_000, 3));
        // Guild B sees only its own first message.
        assert!(!record_message(b, 2, 10_000, 3));
        assert!(record_message(a, 3, 10_000, 3));
        assert!(!record_message(b, 4, 10_000, 3));
    }

    #[test]
    fn duplicates_are_tracked_per_guild() {
        let user = UserId::new(78);
        let (a, b) = ((GuildId::new(8_000_003), user), (GuildId::new(8_000_004), user));
        assert!(!record_duplicate(a, "buy now", 0, 3_000, 2));
        assert!(!record_duplicate(b, "buy now", 1, 3_000, 2));
        assert!(record_duplicate(a, "buy now", 2, 3_000, 2));
    }

    #[test]
    fn forgetting_a_guild_leaves_the_others() {
        let user = UserId::new(79);
        let (a, b) = (GuildId::new(8_000_005), GuildId::new(8_000_006));
        record_message((a, user), 0, 10_000, 100);
        record_message((b, user), 0, 10_000, 100);
        forget_guild(a);
        assert!(!spam_lock().contains_key(&(a, user)));
        assert!(spam_lock().contains_key(&(b, user)));
    }
}
