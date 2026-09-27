//! Deleted-message, bulk-delete, and edit logging, in the ProBot style (see
//! server_logs.rs).
//!
//! Discord does not include content in delete events, so every guild message
//! is written to the database's message store as it arrives (see
//! state/message_store.rs) and read back from there when it is deleted or
//! edited. That survives a restart, which serenity's in-memory cache does not.

use serenity::builder::{CreateAttachment, CreateMessage};
use serenity::client::Context;
use serenity::model::channel::Message;
use serenity::model::event::MessageUpdateEvent;
use serenity::model::id::{ChannelId, GuildId, MessageId};

use crate::state::message_store::{self, StoredMessage};
use crate::systems::server_logs::{build, is_log_channel, log_channel_for, server_of, Who};

/// Remember a new guild message so its content can be logged if it is later
/// deleted or edited, even across a restart.
pub fn on_message(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    if msg.author.id == ctx.cache.current_user().id || is_log_channel(guild_id, msg.channel_id) {
        return;
    }
    message_store::store(&StoredMessage::from_message(msg, guild_id.get()));
}

/// A log that doesn't arrive should say why in the bot's output, not vanish.
fn report<T>(guild_id: GuildId, key: &str, ch: ChannelId, r: serenity::Result<T>) {
    if let Err(e) = r {
        eprintln!("⚠️ [{guild_id}] couldn't post {key} log to channel {ch}: {e}");
    }
}

fn who_of(m: &StoredMessage) -> Who {
    Who { id: m.author_id, tag: m.author_tag.clone(), avatar: m.author_avatar.clone() }
}

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

    // The database copy is what survives a restart; the cache is only a
    // fallback for anything that arrived before the store existed.
    let stored = message_store::mark_deleted(message_id.get()).or_else(|| {
        ctx.cache.message(channel_id, message_id).map(|m| StoredMessage::from_message(&m, guild_id.get()))
    });
    if let Some(m) = &stored {
        if m.author_id == ctx.cache.current_user().id.get() {
            return; // skip my own messages
        }
    }

    let content = match &stored {
        Some(m) => m.content.clone(),
        None => "_content not stored (sent before logging was set up)_".to_string(),
    };
    let author = stored.as_ref().map(who_of);

    // Re-upload attachments so images survive Discord's CDN expiry.
    let mut files = Vec::new();
    let mut lines = Vec::new();
    let mut first_image: Option<String> = None;
    if let Some(m) = &stored {
        for (idx, att) in m.attachments.iter().enumerate() {
            let safe: String = format!(
                "{idx}_{}",
                att.filename.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' }).collect::<String>()
            );
            let downloaded = download(&att.url).await;
            let got_file = downloaded.is_some();
            if let Some(data) = downloaded {
                files.push(CreateAttachment::bytes(data, safe.clone()));
            }
            lines.push(format!("{} · {} KB", att.filename, att.size / 1024));
            let is_image = att.content_type.as_deref().map(|t| t.starts_with("image/")).unwrap_or(false)
                || ["png", "jpg", "jpeg", "gif", "webp"]
                    .iter()
                    .any(|ext| att.filename.to_lowercase().ends_with(&format!(".{ext}")));
            if first_image.is_none() && is_image && got_file {
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
    report(guild_id, "messageDelete", log_ch, log_ch.send_message(&ctx.http, payload).await);
}

/// Fetch an attachment's bytes. Discord pulls the file some time after the
/// message goes, so this can fail; the log still lists the name and size.
async fn download(url: &str) -> Option<Vec<u8>> {
    let resp = reqwest::get(url).await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

pub async fn on_message_delete_bulk(
    ctx: &Context,
    channel_id: ChannelId,
    ids: &[MessageId],
    guild_id: Option<GuildId>,
) {
    let Some(guild_id) = guild_id else { return };
    let Some(log_ch) = log_channel(guild_id, channel_id, "messageDelete") else { return };

    let cached: Vec<StoredMessage> = ids
        .iter()
        .filter_map(|id| {
            message_store::mark_deleted(id.get()).or_else(|| {
                ctx.cache.message(channel_id, *id).map(|m| StoredMessage::from_message(&m, guild_id.get()))
            })
        })
        .collect();
    let lines = cached
        .iter()
        .take(15)
        .map(|m| {
            let body = if m.content.is_empty() { "[embed/attachment]".to_string() } else { truncate(&m.content, 80) };
            format!("<@{}>: {body}", m.author_id)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let more = if cached.len() > 15 { format!("\n…and {} more stored", cached.len() - 15) } else { String::new() };
    let e = build::message_bulk_delete(&server_of(ctx, guild_id), channel_id.get(), ids.len(), &format!("{lines}{more}"));
    report(guild_id, "messageEdit", log_ch, log_ch.send_message(&ctx.http, CreateMessage::new().embed(e)).await);
}

pub async fn on_message_update(ctx: &Context, old: Option<&Message>, new: Option<&Message>, event: &MessageUpdateEvent) {
    // Discord sends an update without content for embed resolves, pins and
    // the like: nothing was edited.
    let Some(after) = event.content.clone().or_else(|| new.map(|m| m.content.clone())) else { return };
    let Some(guild_id) = event.guild_id.or_else(|| new.and_then(|m| m.guild_id)) else { return };
    let channel_id = event.channel_id;
    let me = ctx.cache.current_user().id;

    // What it said before: the database first, since the cache is empty after
    // a restart.
    let stored = message_store::get(event.id.get());
    let before = stored.as_ref().map(|m| m.content.clone()).or_else(|| old.map(|o| o.content.clone()));
    if before.as_deref() == Some(after.as_str()) {
        return;
    }
    if stored.is_some() {
        message_store::update_content(event.id.get(), &after);
    }

    let author = event
        .author
        .as_ref()
        .or_else(|| new.map(|m| &m.author))
        .map(Who::from_user)
        .or_else(|| stored.as_ref().map(who_of));
    let Some(author) = author else { return };
    if author.id == me.get() {
        return;
    }
    let Some(log_ch) = log_channel(guild_id, channel_id, "messageEdit") else { return };

    let before = before.unwrap_or_else(|| "_not stored (sent before logging was set up)_".to_string());
    let url = format!("https://discord.com/channels/{}/{}/{}", guild_id, channel_id, event.id);
    let e = build::message_edit(&server_of(ctx, guild_id), &author, channel_id.get(), &url, &before, &after);
    report(guild_id, "messageEdit", log_ch, log_ch.send_message(&ctx.http, CreateMessage::new().embed(e)).await);
}
