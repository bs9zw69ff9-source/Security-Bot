//! Ticket System: panel, per-type private channels, claim/close, and a
//! self-contained HTML transcript written to that type's log channel.

use serenity::builder::{
    CreateActionRow, CreateAttachment, CreateButton, CreateChannel, CreateEmbed, CreateEmbedFooter, CreateInputText,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateModal, EditMessage,
};
use serenity::client::Context;
use serenity::model::application::{ButtonStyle, ComponentInteraction, InputTextStyle, ModalInteraction};
use serenity::model::channel::{ChannelType, PermissionOverwrite, PermissionOverwriteType};
use serenity::model::id::{ChannelId, GuildId, MessageId, RoleId, UserId};
use serenity::model::{Permissions, Timestamp};

use crate::common::config::now_ms;
use crate::common::embeds::{colors, embed, format_uptime, message_still_exists, sec_log};
use crate::common::permissions::is_mod;
use crate::common::guildinfo::fetch_member;
use crate::state::guild_settings::gc;
use crate::state::tickets::{
    delete_open_ticket, find_open_ticket_by_user, get_open_ticket, get_ticket_config, set_open_ticket,
    update_ticket_config, OpenTicket, TicketConfig,
};

pub fn build_ticket_panel_embed(guild_name: &str, icon_url: Option<String>, cfg: &TicketConfig) -> CreateEmbed {
    let list = cfg
        .types
        .iter()
        .map(|t| format!("{}  **{}**", if t.emoji.is_empty() { "🎫" } else { &t.emoji }, t.label))
        .collect::<Vec<_>>()
        .join("\n");
    let mut e = CreateEmbed::new()
        .color(colors::INFO)
        .title("🎫 Support Tickets")
        .description(format!(
            "Need a hand? Pick the option below that fits what you need, and I'll open a private ticket just for you and the team.\n\n{list}\n\nSomeone will be with you as soon as they can. Please stick to one ticket at a time."
        ))
        .footer(CreateEmbedFooter::new(guild_name))
        .timestamp(Timestamp::now());
    if let Some(url) = icon_url {
        e = e.thumbnail(url);
    }
    e
}

pub fn build_ticket_panel_rows(cfg: &TicketConfig) -> Vec<CreateActionRow> {
    cfg.types
        .iter()
        .take(25)
        .map(|t| {
            let mut b = CreateButton::new(format!("ticket_open_{}", t.key))
                .label(t.label.clone())
                .style(ButtonStyle::Secondary);
            if !t.emoji.is_empty() {
                if let Ok(emoji) = t.emoji.parse::<serenity::model::channel::ReactionType>() {
                    b = b.emoji(emoji);
                }
            }
            b
        })
        .collect::<Vec<_>>()
        .chunks(5)
        .map(|chunk| CreateActionRow::Buttons(chunk.to_vec()))
        .collect()
}

/// Post the panel (or leave it alone if it's already up and the message still
/// exists) - called on boot for every guild with ticket types configured.
pub async fn ensure_ticket_panel(ctx: &Context, guild_id: GuildId) {
    let cfg = get_ticket_config(&guild_id.to_string());
    if cfg.types.is_empty() || cfg.panel_channel_id.is_empty() {
        return;
    }
    let Ok(raw) = cfg.panel_channel_id.parse::<u64>() else { return };
    let channel = ChannelId::new(raw);

    // Only post a replacement when Discord confirms the old panel is gone. An
    // error that is not a 404 means we could not tell, and posting anyway is
    // how a channel ends up with a stack of identical panels.
    if !cfg.panel_message_id.is_empty() {
        if let Ok(mid) = cfg.panel_message_id.parse::<u64>() {
            if message_still_exists(ctx, channel, MessageId::new(mid)).await {
                return;
            }
        }
    }

    let (name, icon) = guild_meta(ctx, guild_id);
    let payload = CreateMessage::new()
        .embed(build_ticket_panel_embed(&name, icon, &cfg))
        .components(build_ticket_panel_rows(&cfg));
    if let Ok(posted) = channel.send_message(&ctx.http, payload).await {
        update_ticket_config(&guild_id.to_string(), |c| c.panel_message_id = posted.id.to_string());
        println!("🎫 Posted ticket panel in #{name}");
    }
}

pub fn guild_meta(ctx: &Context, guild_id: GuildId) -> (String, Option<String>) {
    ctx.cache
        .guild(guild_id)
        .map(|g| (g.name.to_string(), g.icon_url()))
        .unwrap_or_else(|| (guild_id.to_string(), None))
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Paginate through the whole channel history, oldest first.
async fn fetch_all_messages(ctx: &Context, channel: ChannelId) -> Vec<serenity::model::channel::Message> {
    let mut all = Vec::new();
    let mut last: Option<MessageId> = None;
    loop {
        let mut req = serenity::builder::GetMessages::new().limit(100);
        if let Some(id) = last {
            req = req.before(id);
        }
        let Ok(batch) = channel.messages(&ctx.http, req).await else { break };
        if batch.is_empty() {
            break;
        }
        let n = batch.len();
        last = batch.last().map(|m| m.id);
        all.extend(batch);
        if n < 100 {
            break;
        }
    }
    all.reverse();
    all
}

/// Self-contained, dependency-free HTML transcript (dark-themed to resemble
/// Discord).
async fn build_transcript(
    ctx: &Context,
    channel: ChannelId,
    channel_name: &str,
    ticket: &OpenTicket,
    type_label: &str,
    closer_tag: &str,
) -> String {
    let messages = fetch_all_messages(ctx, channel).await;
    let rows = messages
        .iter()
        .map(|m| {
            let secs = m.timestamp.unix_timestamp();
            let time = format_utc(secs);
            let author = escape_html(&m.author.tag());
            let avatar = escape_html(&m.author.face());
            let content = escape_html(&m.content).replace('\n', "<br>");
            let atts = m
                .attachments
                .iter()
                .map(|a| {
                    format!(
                        "<div class=\"att\"><a href=\"{}\" target=\"_blank\" rel=\"noopener\">📎 {}</a></div>",
                        escape_html(&a.url),
                        escape_html(&a.filename)
                    )
                })
                .collect::<String>();
            format!(
                "<div class=\"msg\"><img class=\"avatar\" src=\"{avatar}\"><div class=\"body\"><div class=\"meta\"><span class=\"author\">{author}</span><span class=\"time\">{time}</span></div><div class=\"content\">{}</div>{atts}</div></div>",
                if content.is_empty() { "<i>(no text content)</i>".to_string() } else { content }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Transcript - #{name}</title>
<style>
  body {{ background:#313338; color:#dbdee1; font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif; margin:0; padding:24px; }}
  .header {{ border-bottom:1px solid #3f4147; padding-bottom:16px; margin-bottom:16px; }}
  .header h1 {{ margin:0 0 4px; font-size:20px; color:#f2f3f5; }}
  .header .sub {{ color:#949ba4; font-size:13px; }}
  .msg {{ display:flex; gap:12px; padding:8px 0; }}
  .avatar {{ width:40px; height:40px; border-radius:50%; flex-shrink:0; background:#5865f2; }}
  .meta {{ font-size:13px; margin-bottom:2px; }}
  .author {{ font-weight:600; color:#f2f3f5; }}
  .time {{ color:#949ba4; margin-left:8px; }}
  .content {{ font-size:15px; line-height:1.4; white-space:pre-wrap; word-wrap:break-word; }}
  .att {{ margin-top:4px; }}
  .att a {{ color:#00a8fc; text-decoration:none; }}
</style></head>
<body>
  <div class="header">
    <h1>🎫 {label} - #{name}</h1>
    <div class="sub">Opened by &lt;{opener}&gt; · Closed by {closer} · {count} message(s)</div>
  </div>
  {rows}
</body></html>"#,
        name = escape_html(channel_name),
        label = escape_html(type_label),
        opener = escape_html(&ticket.opener_id),
        closer = escape_html(closer_tag),
        count = messages.len(),
        rows = if rows.is_empty() { "<p><i>No messages were sent in this ticket.</i></p>".to_string() } else { rows },
    )
}

/// `YYYY-MM-DD HH:MM:SS UTC`, matching the JS transcript's timestamp format.
fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

async fn ephemeral(ctx: &Context, i: &ComponentInteraction, content: &str) {
    let _ = i
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content).ephemeral(true)),
        )
        .await;
}

/// Panel button → ask what they need in a modal.
pub async fn handle_ticket_open(ctx: &Context, i: &ComponentInteraction) {
    let Some(guild_id) = i.guild_id else { return };
    let key = i.data.custom_id.trim_start_matches("ticket_open_").to_string();
    let cfg = get_ticket_config(&guild_id.to_string());
    let Some(t) = cfg.types.iter().find(|t| t.key == key) else {
        return ephemeral(ctx, i, "Sorry, that ticket option isn't available anymore.").await;
    };

    if let Some(existing) = find_open_ticket_by_user(&guild_id.to_string(), &i.user.id.to_string(), &key) {
        if ctx.cache.guild(guild_id).map(|g| g.channels.contains_key(&ChannelId::new(existing.parse().unwrap_or(0)))).unwrap_or(false) {
            return ephemeral(ctx, i, &format!("You've already got one open over here: <#{existing}>")).await;
        }
    }

    let modal = CreateModal::new(format!("ticket_reason_{key}"), truncate(&format!("{} - Ticket", t.label), 45))
        .components(vec![CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "What can we help you with?", "reason")
                .required(true)
                .max_length(1000)
                .placeholder("A few details go a long way (who, what, when)..."),
        )]);
    let _ = i.create_response(&ctx.http, CreateInteractionResponse::Modal(modal)).await;
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Modal submit → actually create the private channel.
/// Open a ticket for a user. Shared by the Discord modal and the web
/// dashboard, so both produce an identical channel, welcome embed and log
/// entry rather than drifting apart.
///
/// Returns the new channel on success, or a message safe to show the user.
pub async fn open_ticket(
    ctx: &Context,
    guild_id: GuildId,
    user: &serenity::model::user::User,
    key: &str,
    reason: &str,
) -> Result<ChannelId, String> {
    let cfg = get_ticket_config(&guild_id.to_string());
    let Some(t) = cfg.types.iter().find(|t| t.key == key).cloned() else {
        return Err("Sorry, that ticket option isn't available anymore.".to_string());
    };

    if let Some(existing) = find_open_ticket_by_user(&guild_id.to_string(), &user.id.to_string(), key) {
        return Err(format!("You've already got one open over here: <#{existing}>"));
    }

    // Resolve (or create) the category tickets live under.
    let mut category: Option<ChannelId> = cfg.category_id.parse::<u64>().ok().map(ChannelId::new);
    if category.map(|c| ctx.cache.guild(guild_id).map(|g| !g.channels.contains_key(&c)).unwrap_or(false)).unwrap_or(true) {
        let found = ctx.cache.guild(guild_id).and_then(|g| {
            g.channels.iter().find(|(_, c)| c.kind == ChannelType::Category && c.name == "Tickets").map(|(id, _)| *id)
        });
        category = match found {
            Some(id) => Some(id),
            None => guild_id
                .create_channel(
                    &ctx.http,
                    CreateChannel::new("Tickets")
                        .kind(ChannelType::Category)
                        .audit_log_reason("Ticket system: auto-created category"),
                )
                .await
                .ok()
                .map(|c| c.id),
        };
        if let Some(c) = category {
            update_ticket_config(&guild_id.to_string(), |cfg| cfg.category_id = c.to_string());
        }
    }

    let g = gc(&guild_id.to_string());
    let mut overwrites = vec![
        PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::VIEW_CHANNEL,
            kind: PermissionOverwriteType::Role(RoleId::new(guild_id.get())),
        },
        PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL
                | Permissions::SEND_MESSAGES
                | Permissions::READ_MESSAGE_HISTORY
                | Permissions::ATTACH_FILES,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Member(user.id),
        },
    ];
    if let Ok(mr) = g.mod_role_id.parse::<u64>() {
        overwrites.push(PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL
                | Permissions::SEND_MESSAGES
                | Permissions::READ_MESSAGE_HISTORY
                | Permissions::MANAGE_MESSAGES,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Role(RoleId::new(mr)),
        });
    }

    let safe_name: String = user
        .name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .take(20)
        .collect();
    let safe_name = if safe_name.is_empty() { "user".to_string() } else { safe_name };
    let channel_name = truncate(&format!("{}-{safe_name}", t.key.replace('_', "-")), 90);
    let topic = format!("{} ticket for {} ({})", t.label, user.tag(), user.id);

    let audit_reason = format!("Ticket opened by {}", user.tag());
    let mut builder = CreateChannel::new(channel_name.clone())
        .kind(ChannelType::Text)
        .permissions(overwrites.clone())
        .topic(topic.clone())
        .audit_log_reason(&audit_reason);
    if let Some(cat) = category {
        builder = builder.category(cat);
    }
    let mut created = guild_id.create_channel(&ctx.http, builder).await;
    // If it failed while assigned to a category, retry once without a parent -
    // covers a full/invalid/stale category without fully blocking creation.
    if created.is_err() && category.is_some() {
        created = guild_id
            .create_channel(
                &ctx.http,
                CreateChannel::new(channel_name)
                    .kind(ChannelType::Text)
                    .permissions(overwrites)
                    .topic(topic)
                    .audit_log_reason(&audit_reason),
            )
            .await;
    }

    let ticket_channel = match created {
        Ok(c) => c,
        Err(e) => {
            return Err(format!(
                "Hmm, I couldn't open a ticket channel: `{e}`. Please double-check I have the Manage Channels permission."
            ));
        }
    };

    set_open_ticket(
        &guild_id.to_string(),
        &ticket_channel.id.to_string(),
        OpenTicket {
            type_key: key.to_string(),
            opener_id: user.id.to_string(),
            opened_at: now_ms(),
            claimed_by: None,
            reason: reason.to_string(),
        },
    );

    let welcome = CreateEmbed::new()
        .color(colors::INFO)
        .title(format!("{} {}", if t.emoji.is_empty() { "🎫" } else { &t.emoji }, t.label))
        .description(format!(
            "Thanks for reaching out, <@{}> - someone from the team will be with you shortly. Here's what you told us:\n\n{reason}",
            user.id
        ))
        .field("Opened by", format!("<@{}>", user.id), true)
        .field("Category", t.label.clone(), true)
        .field("Status", "🟢 Open, waiting for staff", true)
        .footer(CreateEmbedFooter::new(format!("Ticket ID: {}", ticket_channel.id)))
        .timestamp(Timestamp::now());
    let controls = CreateActionRow::Buttons(vec![
        CreateButton::new("ticket_claim").label("Claim").emoji('🙋').style(ButtonStyle::Primary),
        CreateButton::new("ticket_close").label("Close Ticket").emoji('🔒').style(ButtonStyle::Danger),
    ]);
    let ping = if g.mod_role_id.is_empty() { String::new() } else { format!("<@&{}> ", g.mod_role_id) };
    let _ = ticket_channel
        .id
        .send_message(
            &ctx.http,
            CreateMessage::new()
                .content(format!("{ping}<@{}>", user.id))
                .embed(welcome)
                .components(vec![controls]),
        )
        .await;

    sec_log(
        ctx,
        guild_id,
        "Ticket Opened",
        &format!("<@{}> opened a **{}** ticket over in <#{}>.", user.id, t.label, ticket_channel.id),
        colors::INFO,
    )
    .await;
    Ok(ticket_channel.id)
}


/// Discord modal path: open the ticket, then report the outcome in the
/// ephemeral reply.
pub async fn create_ticket_channel(ctx: &Context, i: &ModalInteraction, key: &str, reason: &str) {
    let Some(guild_id) = i.guild_id else { return };
    let _ = i
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new().ephemeral(true)),
        )
        .await;
    let msg = match open_ticket(ctx, guild_id, &i.user, key, reason).await {
        Ok(id) => format!("You're all set - your ticket's open here: <#{id}>"),
        Err(why) => why,
    };
    let _ = i
        .edit_response(&ctx.http, serenity::builder::EditInteractionResponse::new().content(msg))
        .await;
}

pub async fn handle_ticket_claim(ctx: &Context, i: &ComponentInteraction) {
    let Some(guild_id) = i.guild_id else { return };
    let Some(mut ticket) = get_open_ticket(&guild_id.to_string(), &i.channel_id.to_string()) else {
        return ephemeral(ctx, i, "This isn't an active ticket channel.").await;
    };
    let owner_id = ctx.cache.guild(guild_id).map(|g| g.owner_id).unwrap_or(UserId::new(1));
    let Some(member) = fetch_member(ctx, guild_id, i.user.id).await else { return };
    if !is_mod(&member, owner_id) {
        return ephemeral(ctx, i, "Only staff can claim tickets.").await;
    }
    if let Some(by) = &ticket.claimed_by {
        return ephemeral(ctx, i, &format!("This one's already claimed by <@{by}>.")).await;
    }

    ticket.claimed_by = Some(i.user.id.to_string());
    set_open_ticket(&guild_id.to_string(), &i.channel_id.to_string(), ticket);

    // Repaint the status field in place, keeping the rest of the embed.
    if let Some(old) = i.message.embeds.first() {
        let mut e = CreateEmbed::new().color(colors::INFO);
        if let Some(t) = &old.title {
            e = e.title(t.clone());
        }
        if let Some(d) = &old.description {
            e = e.description(d.clone());
        }
        for (idx, f) in old.fields.iter().enumerate() {
            if idx == 2 {
                e = e.field("Status", format!("🟡 Claimed by <@{}>", i.user.id), true);
            } else {
                e = e.field(f.name.clone(), f.value.clone(), f.inline);
            }
        }
        if let Some(f) = &old.footer {
            e = e.footer(CreateEmbedFooter::new(f.text.clone()));
        }
        let _ = i
            .create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(CreateInteractionResponseMessage::new().embed(e)))
            .await;
    } else {
        let _ = i.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
    }

    let _ = i
        .channel_id
        .send_message(
            &ctx.http,
            CreateMessage::new().embed(embed(
                colors::WARN,
                format!("<@{}> has got this one and will help you out from here.", i.user.id),
                None,
            )),
        )
        .await;
}

pub async fn handle_ticket_close(ctx: &Context, i: &ComponentInteraction) {
    let Some(guild_id) = i.guild_id else { return };
    let Some(ticket) = get_open_ticket(&guild_id.to_string(), &i.channel_id.to_string()) else {
        return ephemeral(ctx, i, "This isn't an active ticket channel.").await;
    };
    let owner_id = ctx.cache.guild(guild_id).map(|g| g.owner_id).unwrap_or(UserId::new(1));
    let Some(member) = fetch_member(ctx, guild_id, i.user.id).await else { return };
    if !is_mod(&member, owner_id) && i.user.id.to_string() != ticket.opener_id {
        return ephemeral(ctx, i, "Only staff or the person who opened this can close it.").await;
    }

    let _ = i
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().embed(embed(
                colors::WARN,
                "Closing this ticket and saving a transcript, one sec...",
                None,
            ))),
        )
        .await;

    let cfg = get_ticket_config(&guild_id.to_string());
    let t = cfg.types.iter().find(|t| t.key == ticket.type_key).cloned();
    let label = t.as_ref().map(|t| t.label.clone()).unwrap_or_else(|| ticket.type_key.clone());
    let channel_name = ctx
        .cache
        .guild(guild_id)
        .and_then(|g| g.channels.get(&i.channel_id).map(|c| c.name.to_string()))
        .unwrap_or_else(|| i.channel_id.to_string());

    let transcript = build_transcript(ctx, i.channel_id, &channel_name, &ticket, &label, &i.user.tag()).await;

    let opener_tag = ticket
        .opener_id
        .parse::<u64>()
        .ok()
        .map(UserId::new)
        .map(|id| async move { id.to_user(&ctx.http).await.ok().map(|u| u.tag()) });
    let opener_tag = match opener_tag {
        Some(fut) => fut.await,
        None => None,
    };

    let summary = CreateEmbed::new()
        .color(colors::NEUTRAL)
        .title(format!("🔒 Ticket Closed - {label}"))
        .field(
            "Opened by",
            match &opener_tag {
                Some(tag) => format!("{tag} (`{}`)", ticket.opener_id),
                None => format!("`{}`", ticket.opener_id),
            },
            true,
        )
        .field("Closed by", format!("<@{}>", i.user.id), true)
        .field(
            "Claimed by",
            ticket.claimed_by.as_ref().map(|c| format!("<@{c}>")).unwrap_or_else(|| "Unclaimed".to_string()),
            true,
        )
        .field("Opened", format!("<t:{}:F>", ticket.opened_at / 1000), true)
        .field("Duration", format_uptime(now_ms() - ticket.opened_at), true)
        .field("Reason", truncate(if ticket.reason.is_empty() { "N/A" } else { &ticket.reason }, 1024), false)
        .timestamp(Timestamp::now());

    if let Some(log_id) = t.as_ref().and_then(|t| t.log_channel_id.parse::<u64>().ok()) {
        let _ = ChannelId::new(log_id)
            .send_message(
                &ctx.http,
                CreateMessage::new().embed(summary).add_file(CreateAttachment::bytes(
                    transcript.into_bytes(),
                    format!("transcript-{channel_name}.html"),
                )),
            )
            .await;
    }

    sec_log(
        ctx,
        guild_id,
        "Ticket Closed",
        &format!(
            "<@{}> closed the **{label}** ticket that <@{}> opened (<#{}>).",
            i.user.id, ticket.opener_id, i.channel_id
        ),
        colors::NEUTRAL,
    )
    .await;
    delete_open_ticket(&guild_id.to_string(), &i.channel_id.to_string());

    let _ = i.channel_id.say(&ctx.http, "All done here - this channel will disappear in a few seconds.").await;
    let http = ctx.http.clone();
    let channel = i.channel_id;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let _ = channel.delete(&http).await;
    });
}

/// Re-render an existing panel message in place, or post a fresh one.
pub async fn post_or_edit_panel(ctx: &Context, guild_id: GuildId, channel: ChannelId, cfg: &TicketConfig) -> bool {
    let (name, icon) = guild_meta(ctx, guild_id);
    let e = build_ticket_panel_embed(&name, icon, cfg);
    let rows = build_ticket_panel_rows(cfg);

    if cfg.panel_channel_id == channel.to_string() && !cfg.panel_message_id.is_empty() {
        if let Ok(mid) = cfg.panel_message_id.parse::<u64>() {
            if let Ok(mut msg) = channel.message(&ctx.http, MessageId::new(mid)).await {
                if msg.edit(&ctx.http, EditMessage::new().embed(e.clone()).components(rows.clone())).await.is_ok() {
                    update_ticket_config(&guild_id.to_string(), |c| {
                        c.panel_channel_id = channel.to_string();
                        c.panel_message_id = msg.id.to_string();
                    });
                    return true;
                }
            }
        }
    }
    match channel.send_message(&ctx.http, CreateMessage::new().embed(e).components(rows)).await {
        Ok(posted) => {
            update_ticket_config(&guild_id.to_string(), |c| {
                c.panel_channel_id = channel.to_string();
                c.panel_message_id = posted.id.to_string();
            });
            true
        }
        Err(_) => false,
    }
}
