//! Anti-Spam: mass mentions, scam/grabber links, invite links, duplicate
//! floods, and raw message-frequency floods.
//!
//! Trackers are deliberately in-memory only: their windows are seconds, so a
//! restart naturally (and safely) interrupting a fast burst is fine.

use once_cell::sync::Lazy;
use serenity::client::Context;
use serenity::model::channel::Message;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{now_ms, CONFIG, INVITE_RE, SCAM_RE};
use crate::common::embeds::{alert_owner, colors, sec_log};
use crate::common::guildinfo::{fetch_member, GuildInfo};
use crate::common::permissions::{is_mod, is_whitelisted};
use super::mute::mute_user;

/// "gid:uid" -> recent message timestamps
pub static SPAM_TRACKER: Lazy<Mutex<HashMap<String, Vec<i64>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
pub struct Dupe {
    pub content: String,
    pub count: usize,
    pub ts: i64,
}
/// "gid:uid" -> last message + repeat count
pub static DUPE_TRACKER: Lazy<Mutex<HashMap<String, Dupe>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn spam_lock() -> std::sync::MutexGuard<'static, HashMap<String, Vec<i64>>> {
    match SPAM_TRACKER.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}
fn dupe_lock() -> std::sync::MutexGuard<'static, HashMap<String, Dupe>> {
    match DUPE_TRACKER.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// Returns true when the message was handled as spam (so anti-ping is skipped).
pub async fn check_spam(ctx: &Context, msg: &Message, info: &GuildInfo) -> bool {
    let Some(member) = fetch_member(ctx, info.id, msg.author.id).await else { return false };
    // Set SPAM_EXEMPT_STAFF=false to stress-test on your own staff account.
    if CONFIG.spam_exempt_staff && (is_mod(&member, info.owner_id) || is_whitelisted(&member, info.owner_id)) {
        return false;
    }
    let uid = msg.author.id;
    let key = format!("{}:{}", info.id, uid);
    let now = now_ms();
    let channel = msg.channel_id;

    // Mass-mention in a single message (@everyone / @here counts as mass)
    let mention_count = msg.mentions.len()
        + msg.mention_roles.len()
        + if msg.mention_everyone { CONFIG.spam_mention_limit } else { 0 };
    if mention_count >= CONFIG.spam_mention_limit {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, CONFIG.spam_mute_min, &format!("Anti-spam: mass mention ({mention_count})")).await;
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
    if CONFIG.scam_block && SCAM_RE.is_match(&msg.content) {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, CONFIG.spam_mute_min, "Anti-spam: scam/grabber link").await;
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
    if CONFIG.spam_block_invites && INVITE_RE.is_match(&msg.content) && !is_mod(&member, info.owner_id) {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, CONFIG.spam_mute_min, "Anti-spam: posted invite link").await;
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
    let dupe_tripped = {
        let mut map = dupe_lock();
        match map.get_mut(&key) {
            Some(d) if d.content == msg.content && now - d.ts < CONFIG.spam_window_ms * 3 => {
                d.count += 1;
                d.ts = now;
                if d.count >= CONFIG.spam_duplicate_limit {
                    map.insert(key.clone(), Dupe { content: String::new(), count: 0, ts: now });
                    true
                } else {
                    false
                }
            }
            _ => {
                map.insert(key.clone(), Dupe { content: msg.content.to_string(), count: 1, ts: now });
                false
            }
        }
    };
    if dupe_tripped {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, CONFIG.spam_mute_min, "Anti-spam: duplicate flood").await;
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
    let flood_tripped = {
        let mut map = spam_lock();
        let arr = map.entry(key.clone()).or_default();
        arr.retain(|t| now - *t < CONFIG.spam_window_ms);
        arr.push(now);
        if arr.len() >= CONFIG.spam_threshold {
            arr.clear();
            true
        } else {
            false
        }
    };
    if flood_tripped {
        let _ = msg.delete(&ctx.http).await;
        mute_user(ctx, info, &member, CONFIG.spam_mute_min, "Anti-spam: message flood").await;
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

/// Drop tracker entries that have aged well past their window.
pub fn sweep() {
    let now = now_ms();
    spam_lock().retain(|_, arr| arr.last().map(|t| now - *t <= CONFIG.spam_window_ms * 5).unwrap_or(false));
    dupe_lock().retain(|_, d| now - d.ts <= CONFIG.spam_window_ms * 5);
}
