//! Anti-Ping: punish mentions of protected users/roles.

use serenity::builder::CreateMessage;
use serenity::client::Context;
use serenity::model::channel::Message;
use serenity::model::Timestamp;
use std::collections::BTreeSet;

use crate::common::config::now_ms;
use crate::common::embeds::{colors, embed, render_anti_ping_response, sec_log};
use crate::common::guildinfo::{fetch_member, GuildInfo};
use crate::common::permissions::{is_mod, is_whitelisted};
use crate::state::anti_ping::ap;
use super::mute::mute_user;

pub async fn check_anti_ping(ctx: &Context, msg: &Message, info: &GuildInfo) {
    let a = ap(&info.id.to_string());
    if !a.enabled {
        return;
    }
    let Some(member) = fetch_member(ctx, info.id, msg.author.id).await else { return };
    if member.user.id == info.owner_id {
        return;
    }
    if is_mod(&member, info.owner_id) || is_whitelisted(&member, info.owner_id) {
        return;
    }

    // BTreeSet keeps the reported targets in a stable order.
    let mut hits: BTreeSet<String> = BTreeSet::new();
    let replied_to = msg.referenced_message.as_ref().map(|m| m.author.id);
    for user in &msg.mentions {
        if user.id == msg.author.id || user.bot {
            continue;
        }
        if a.ignore_replies && replied_to == Some(user.id) {
            continue;
        }
        if a.protected_users.contains(&user.id.to_string()) {
            hits.insert(format!("<@{}>", user.id));
            continue;
        }
        if let Some(t) = fetch_member(ctx, info.id, user.id).await {
            if t.roles.iter().any(|r| a.protected_roles.contains(&r.to_string())) {
                hits.insert(format!("<@{}>", user.id));
            }
        }
    }
    for rid in &msg.mention_roles {
        if a.protected_roles.contains(&rid.to_string()) {
            hits.insert(format!("<@&{rid}>"));
        }
    }
    if hits.is_empty() {
        return;
    }

    let targets = hits.into_iter().collect::<Vec<_>>().join(", ");
    let reason = format!("Anti-ping: mentioned protected {targets}");
    if a.delete_message {
        let _ = msg.delete(&ctx.http).await;
    }

    let action_text = match a.action.as_str() {
        "mute" => {
            mute_user(ctx, info, &member, a.timeout_min, &reason).await;
            format!("muted for {} min", a.timeout_min)
        }
        "timeout" => {
            let until = Timestamp::from_millis(now_ms() + a.timeout_min * 60_000).unwrap_or_else(|_| Timestamp::now());
            let mut m = member.clone();
            let _ = m.disable_communication_until_datetime(&ctx.http, until).await;
            format!("timed out for {} min", a.timeout_min)
        }
        "warn" => "warned".to_string(),
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
