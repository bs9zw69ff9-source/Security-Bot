//! Deleted-message, bulk-delete, and edit logging, in the ProBot style (see
//! server_logs.rs).
//!
//! Discord does not include content in delete events, so the original message
//! is recovered from serenity's message cache where possible - the same
//! "_content not cached (sent before restart)_" caveat the JS bot had.

use serenity::builder::{CreateAttachment, CreateMessage};
use serenity::client::Context;
use serenity::model::channel::Message;
use serenity::model::id::{ChannelId, GuildId, MessageId};

use crate::systems::server_logs::{build, is_log_channel, log_channel_for, server_of, Who};

/// Resolve where a message log goes (its `/setup logs` channel, else the
/// legacy message-log channel), skipping anything that is itself a log channel.
fn log_channel(guild_id: GuildId, source: ChannelId, key: &str) -> Option<ChannelId> {
    if is_log_channel(guild_id, source) {
        return None; // don't log the log channels themselves
    }
    log_channel_for(guild_id, key)
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

pub async fn on_message_delete(ctx: &Context, channel_id: ChannelId, message_id: MessageId, guild_id: Option<GuildId>) {
    let Some(guild_id) = guild_id else { return };
    let Some(log_ch) = log_channel(guild_id, channel_id, "messageDelete") else { return };

    let cached: Option<Message> = ctx.cache.message(channel_id, message_id).map(|m| m.clone());
    if let Some(m) = &cached {
        if m.author.id == ctx.cache.current_user().id {
            return; // skip my own messages
        }
    }

    let content = match &cached {
        Some(m) => m.content.clone(),
        None => "_content not cached (sent before restart)_".to_string(),
    };
    let author = cached.as_ref().map(|m| Who::from_user(&m.author));

    // Re-upload attachments so images survive Discord's CDN expiry.
    let mut files = Vec::new();
    let mut lines = Vec::new();
    let mut first_image: Option<String> = None;
    if let Some(m) = &cached {
        for (idx, att) in m.attachments.iter().enumerate() {
            let safe: String = format!(
                "{idx}_{}",
                att.filename.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' }).collect::<String>()
            );
            if let Ok(data) = att.download().await {
                files.push(CreateAttachment::bytes(data, safe.clone()));
            }
            lines.push(format!("{} · {} KB", att.filename, att.size / 1024));
            let is_image = att.content_type.as_deref().map(|t| t.starts_with("image/")).unwrap_or(false)
                || ["png", "jpg", "jpeg", "gif", "webp"]
                    .iter()
                    .any(|ext| att.filename.to_lowercase().ends_with(&format!(".{ext}")));
            if first_image.is_none() && is_image {
                first_image = Some(safe);
            }
        }
    }
    let mut e = build::message_delete(&server_of(ctx, guild_id), author.as_ref(), channel_id.get(), &content, &lines);
    if let Some(img) = &first_image {
        e = e.attachment(img.clone());
    }

    let mut payload = CreateMessage::new().embed(e);
    for f in files {
        payload = payload.add_file(f);
    }
    let _ = log_ch.send_message(&ctx.http, payload).await;
}

pub async fn on_message_delete_bulk(
    ctx: &Context,
    channel_id: ChannelId,
    ids: &[MessageId],
    guild_id: Option<GuildId>,
) {
    let Some(guild_id) = guild_id else { return };
    let Some(log_ch) = log_channel(guild_id, channel_id, "messageDelete") else { return };

    let cached: Vec<Message> =
        ids.iter().filter_map(|id| ctx.cache.message(channel_id, *id).map(|m| m.clone())).collect();
    let lines = cached
        .iter()
        .take(15)
        .map(|m| {
            let body = if m.content.is_empty() { "[embed/attachment]".to_string() } else { truncate(&m.content, 80) };
            format!("<@{}>: {body}", m.author.id)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let more = if cached.len() > 15 { format!("\n…and {} more cached", cached.len() - 15) } else { String::new() };
    let e = build::message_bulk_delete(&server_of(ctx, guild_id), channel_id.get(), ids.len(), &format!("{lines}{more}"));
    let _ = log_ch.send_message(&ctx.http, CreateMessage::new().embed(e)).await;
}

pub async fn on_message_update(ctx: &Context, old: Option<&Message>, new: &Message) {
    let Some(guild_id) = new.guild_id else { return };
    let Some(log_ch) = log_channel(guild_id, new.channel_id, "messageEdit") else { return };
    if new.author.id == ctx.cache.current_user().id {
        return;
    }
    // Ignore embed-resolve / pin / other non-content updates.
    if old.map(|o| o.content == new.content).unwrap_or(false) {
        return;
    }

    let before = match old {
        Some(o) => o.content.clone(),
        None => "_not cached (sent before restart)_".to_string(),
    };
    let e = build::message_edit(
        &server_of(ctx, guild_id),
        &Who::from_user(&new.author),
        new.channel_id.get(),
        &new.link(),
        &before,
        &new.content,
    );
    let _ = log_ch.send_message(&ctx.http, CreateMessage::new().embed(e)).await;
}
