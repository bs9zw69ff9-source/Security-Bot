//! Deleted-message, bulk-delete, and edit logging.
//!
//! Discord does not include content in delete events, so the original message
//! is recovered from serenity's message cache where possible - the same
//! "_content not cached (sent before restart)_" caveat the JS bot had.

use serenity::builder::{CreateAttachment, CreateEmbed, CreateEmbedAuthor, CreateMessage};
use serenity::client::Context;
use serenity::model::channel::Message;
use serenity::model::id::{ChannelId, GuildId, MessageId};
use serenity::model::Timestamp;

use crate::common::embeds::colors;
use crate::state::guild_settings::gc;

/// Resolve the configured message-log channel, skipping the log channel itself.
fn log_channel(guild_id: GuildId, source: ChannelId) -> Option<ChannelId> {
    let id = gc(&guild_id.to_string()).msg_log_channel_id;
    let raw = id.parse::<u64>().ok()?;
    let ch = ChannelId::new(raw);
    if ch == source {
        return None; // don't log the log channel itself
    }
    Some(ch)
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

pub async fn on_message_delete(ctx: &Context, channel_id: ChannelId, message_id: MessageId, guild_id: Option<GuildId>) {
    let Some(guild_id) = guild_id else { return };
    let Some(log_ch) = log_channel(guild_id, channel_id) else { return };

    let cached: Option<Message> = ctx.cache.message(channel_id, message_id).map(|m| m.clone());
    if let Some(m) = &cached {
        if m.author.id == ctx.cache.current_user().id {
            return; // skip my own messages
        }
    }

    let author_line = match &cached {
        Some(m) => format!("**Author:** <@{}> · `{}` · `{}`\n", m.author.id, m.author.tag(), m.author.id),
        None => "**Author:** _uncached_\n".to_string(),
    };
    let content_line = match &cached {
        Some(m) if !m.content.is_empty() => format!("**Content:**\n{}", truncate(&m.content, 1800)),
        Some(_) => "_no text content_".to_string(),
        None => "_content not cached (sent before restart)_".to_string(),
    };

    let mut e = CreateEmbed::new()
        .color(colors::MUTED)
        .description(format!("🗑️ **Message deleted** in <#{channel_id}>\n{author_line}{content_line}"))
        .timestamp(Timestamp::now());
    if let Some(m) = &cached {
        e = e.author(CreateEmbedAuthor::new(m.author.tag()).icon_url(m.author.face()));
    }

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
        if let Some(img) = &first_image {
            e = e.attachment(img.clone());
        }
        if !m.attachments.is_empty() {
            e = e.field(format!("Attachments ({})", m.attachments.len()), truncate(&lines.join("\n"), 1024), false);
        }
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
    let Some(log_ch) = log_channel(guild_id, channel_id) else { return };

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

    let e = CreateEmbed::new()
        .color(colors::WARN)
        .title("🧹 Bulk delete")
        .description(format!(
            "**{}** messages deleted in <#{channel_id}>{}{}",
            ids.len(),
            if lines.is_empty() { String::new() } else { format!("\n\n{lines}") },
            if cached.len() > 15 { format!("\n…and {} more cached", cached.len() - 15) } else { String::new() }
        ))
        .timestamp(Timestamp::now());
    let _ = log_ch.send_message(&ctx.http, CreateMessage::new().embed(e)).await;
}

pub async fn on_message_update(ctx: &Context, old: Option<&Message>, new: &Message) {
    let Some(guild_id) = new.guild_id else { return };
    let Some(log_ch) = log_channel(guild_id, new.channel_id) else { return };
    if new.author.id == ctx.cache.current_user().id {
        return;
    }
    // Ignore embed-resolve / pin / other non-content updates.
    if old.map(|o| o.content == new.content).unwrap_or(false) {
        return;
    }

    let before = match old {
        Some(o) if !o.content.is_empty() => truncate(&o.content, 1024),
        Some(_) => "_empty_".to_string(),
        None => "_not cached (sent before restart)_".to_string(),
    };
    let after = if new.content.is_empty() { "_empty_".to_string() } else { truncate(&new.content, 1024) };

    let e = CreateEmbed::new()
        .color(colors::INFO)
        .description(format!(
            "✏️ **Message edited** in <#{}> · [jump]({})\n**Author:** <@{}> · `{}` · `{}`",
            new.channel_id,
            new.link(),
            new.author.id,
            new.author.tag(),
            new.author.id
        ))
        .field("Before", before, false)
        .field("After", after, false)
        .author(CreateEmbedAuthor::new(new.author.tag()).icon_url(new.author.face()))
        .timestamp(Timestamp::now());
    let _ = log_ch.send_message(&ctx.http, CreateMessage::new().embed(e)).await;
}
