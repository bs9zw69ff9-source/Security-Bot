//! Application System (Appy-style DM interview → staff review → role grant).

use once_cell::sync::Lazy;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, CreateModal, EditMessage, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption};
use serenity::client::Context;
use serenity::collector::{ComponentInteractionCollector, MessageCollector};
use serenity::model::application::{ButtonStyle, ComponentInteraction, InputTextStyle, ModalInteraction};
use serenity::model::id::{ChannelId, GuildId, MessageId, RoleId, UserId};
use serenity::model::user::User;
use serenity::model::Timestamp;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use crate::common::config::now_ms;
use crate::common::embeds::{colors, sec_log, APPY_BLURPLE, APPY_GREEN, APPY_RED, APP_PENDING};
use crate::common::guildinfo::fetch_member;
use crate::common::permissions::is_mod;
use crate::state::applications::{get_application, get_applications, update_application, Application};

/// Per-question DM reply window.
const APP_QUESTION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Users with an in-progress DM interview, so we never start two at once.
static ACTIVE_DM_APPS: Lazy<Mutex<HashSet<u64>>> = Lazy::new(|| Mutex::new(HashSet::new()));

fn active_lock() -> std::sync::MutexGuard<'static, HashSet<u64>> {
    match ACTIVE_DM_APPS.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// Per-answer character cap: spread a safe budget across the questions so the
/// finished review embed stays under Discord's 6000-char total, capped at the
/// 1024 per-field limit.
fn app_answer_cap(question_count: usize) -> usize {
    (5200 / question_count.max(1)).clamp(200, 1024)
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Requirements block shown as an application panel's description. Age and
/// member-time minimums are per-app, so each application states its own.
pub fn build_requirements(app: &Application) -> String {
    let age = app.min_age.unwrap_or(14);
    let member_time = app.min_member_time.clone().unwrap_or_else(|| "1 week".to_string());
    format!(
        "**REQUIREMENTS**\nAge: {age}\nNo Joke Applications (May result in blacklist)\nUse of AI is not tolerated\nMust be a member longer than {member_time}"
    )
}

fn emoji_or_default(e: &str) -> &str {
    if e.is_empty() {
        "📝"
    } else {
        e
    }
}

pub fn build_app_panel_embed(guild_name: &str, icon: Option<String>, app: &Application) -> CreateEmbed {
    let closed = app.closed;
    let mut e = CreateEmbed::new()
        .color(if closed { colors::NEUTRAL } else { colors::INFO })
        .title(format!(
            "{} {} Application{}",
            emoji_or_default(&app.emoji),
            app.label,
            if closed { " (Closed)" } else { "" }
        ))
        .footer(CreateEmbedFooter::new(guild_name))
        .timestamp(Timestamp::now())
        .description(if closed {
            format!(
                "**{} applications are closed right now.** Check back soon.\n\n{}",
                app.label,
                build_requirements(app)
            )
        } else {
            build_requirements(app)
        });
    if let Some(url) = icon {
        e = e.thumbnail(url);
    }
    e
}

/// A single Apply button reflecting one app's open/closed state.
pub fn build_apply_button(app: &Application) -> CreateButton {
    let closed = app.closed;
    let label = if closed {
        truncate(&format!("{} closed", app.label), 80)
    } else {
        truncate(&format!("Apply for {}", app.label), 80)
    };
    let mut b = CreateButton::new(format!("app_apply_{}", app.key))
        .label(label)
        .style(if closed { ButtonStyle::Secondary } else { ButtonStyle::Primary })
        .disabled(closed);
    let emoji = if closed { "🔒" } else { emoji_or_default(&app.emoji) };
    if let Ok(parsed) = emoji.parse::<serenity::model::channel::ReactionType>() {
        b = b.emoji(parsed);
    }
    b
}

/// Combined panel embed for a channel that hosts 2+ applications - one embed,
/// a button per app. If every app shares the same requirements, show one
/// block; otherwise show each app's requirements under its own heading.
pub fn build_combined_panel_embed(guild_name: &str, icon: Option<String>, apps: &[Application]) -> CreateEmbed {
    let blocks: Vec<(String, String)> =
        apps.iter().map(|a| (format!("{} __{}__", emoji_or_default(&a.emoji), a.label), build_requirements(a))).collect();
    let unique: Vec<&String> = {
        let mut seen: Vec<&String> = Vec::new();
        for (_, r) in &blocks {
            if !seen.contains(&r) {
                seen.push(r);
            }
        }
        seen
    };
    let description = if unique.len() == 1 {
        unique[0].clone()
    } else {
        blocks.iter().map(|(h, r)| format!("{h}\n{r}")).collect::<Vec<_>>().join("\n\n")
    };
    let mut e = CreateEmbed::new()
        .color(colors::INFO)
        .title("📋 Applications")
        .description(description)
        .footer(CreateEmbedFooter::new(guild_name))
        .timestamp(Timestamp::now());
    if let Some(url) = icon {
        e = e.thumbnail(url);
    }
    e
}

/// Group a guild's panel-eligible apps by their panel channel, preserving
/// configuration order.
pub fn apps_by_panel_channel(guild_id: &str) -> Vec<(String, Vec<Application>)> {
    let mut groups: Vec<(String, Vec<Application>)> = Vec::new();
    for app in get_applications(guild_id).values() {
        if app.panel_channel_id.is_empty() || app.questions.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|(c, _)| *c == app.panel_channel_id) {
            Some((_, list)) => list.push(app.clone()),
            None => groups.push((app.panel_channel_id.clone(), vec![app.clone()])),
        }
    }
    groups
}

/// The chooser on a panel hosting more than one application.
///
/// A dropdown rather than a row of buttons: it stays one line however many
/// applications there are, each option gets a description rather than just a
/// label, and it does not run out of room at five the way a button row does.
/// A closed application still appears, marked closed, so people can see it
/// exists rather than wondering where it went.
fn build_apply_select(apps: &[Application]) -> CreateActionRow {
    let options: Vec<CreateSelectMenuOption> = apps
        .iter()
        .take(25)
        .map(|a| {
            let label = if a.closed {
                truncate(&format!("{} (closed)", a.label), 100)
            } else {
                truncate(&a.label, 100)
            };
            let desc = if a.closed {
                "Not accepting applications right now".to_string()
            } else {
                let n = a.questions.len();
                format!("{n} question{}", if n == 1 { "" } else { "s" })
            };
            let mut opt = CreateSelectMenuOption::new(label, a.key.clone()).description(truncate(&desc, 100));
            let emoji = if a.closed { "🔒" } else { emoji_or_default(&a.emoji) };
            if let Ok(parsed) = emoji.parse::<serenity::model::channel::ReactionType>() {
                opt = opt.emoji(parsed);
            }
            opt
        })
        .collect();

    // Every option disabled would leave a menu that does nothing, so say so on
    // the placeholder instead.
    let all_closed = apps.iter().all(|a| a.closed);
    let placeholder = if all_closed {
        "Applications are closed right now"
    } else {
        "Choose what you'd like to apply for"
    };
    CreateActionRow::SelectMenu(
        CreateSelectMenu::new("app_pick", CreateSelectMenuKind::String { options })
            .placeholder(placeholder)
            .disabled(all_closed),
    )
}

fn panel_payload(guild_name: &str, icon: Option<String>, apps: &[Application]) -> (CreateEmbed, Vec<CreateActionRow>) {
    if apps.len() == 1 {
        return (
            build_app_panel_embed(guild_name, icon, &apps[0]),
            vec![CreateActionRow::Buttons(vec![build_apply_button(&apps[0])])],
        );
    }
    (build_combined_panel_embed(guild_name, icon, apps), vec![build_apply_select(apps)])
}

/// Point every app in a channel group at the same panel message id.
fn set_group_panel_message(guild_id: &str, apps: &[Application], message_id: &str) {
    for a in apps {
        if a.panel_message_id != message_id {
            update_application(guild_id, &a.key, |app| app.panel_message_id = message_id.to_string());
        }
    }
}

/// Render (edit-in-place or post) the one panel message for a channel group,
/// so open/close changes on any member app update the shared panel live.
pub async fn render_channel_panel(ctx: &Context, guild_id: GuildId, channel_id: &str, apps: &[Application]) {
    let Ok(raw) = channel_id.parse::<u64>() else { return };
    let channel = ChannelId::new(raw);
    let (name, icon) = super::tickets::guild_meta(ctx, guild_id);
    let (e, rows) = panel_payload(&name, icon, apps);

    if let Some(existing_id) = apps.iter().map(|a| a.panel_message_id.clone()).find(|m| !m.is_empty()) {
        if let Ok(mid) = existing_id.parse::<u64>() {
            if let Ok(mut msg) = channel.message(&ctx.http, MessageId::new(mid)).await {
                if msg.edit(&ctx.http, EditMessage::new().embed(e.clone()).components(rows.clone())).await.is_ok() {
                    set_group_panel_message(&guild_id.to_string(), apps, &msg.id.to_string());
                    return;
                }
            }
        }
    }
    if let Ok(posted) = channel.send_message(&ctx.http, CreateMessage::new().embed(e).components(rows)).await {
        set_group_panel_message(&guild_id.to_string(), apps, &posted.id.to_string());
    }
}

/// Refresh the whole panel of the channel `app` lives in (so a combined
/// panel's other buttons are rebuilt too when this one's state changes).
pub async fn refresh_app_panel(ctx: &Context, guild_id: GuildId, app: &Application) {
    if app.panel_channel_id.is_empty() {
        return;
    }
    let groups = apps_by_panel_channel(&guild_id.to_string());
    let apps = groups
        .into_iter()
        .find(|(c, _)| *c == app.panel_channel_id)
        .map(|(_, a)| a)
        .unwrap_or_else(|| vec![app.clone()]);
    render_channel_panel(ctx, guild_id, &app.panel_channel_id, &apps).await;
}

/// Delete panel messages in every channel except `keep`.
///
/// Used when applications are gathered onto a single panel: the panels they
/// leave behind are still live messages with a working chooser on them, so
/// without this people would keep applying through a panel that no longer
/// reflects the configuration.
pub async fn retire_panels_outside(ctx: &Context, guild_id: GuildId, keep: &str) {
    for (channel_id, apps) in apps_by_panel_channel(&guild_id.to_string()) {
        if channel_id == keep {
            continue;
        }
        let Ok(raw) = channel_id.parse::<u64>() else { continue };
        let channel = ChannelId::new(raw);
        let mut seen: Vec<String> = Vec::new();
        for a in &apps {
            if a.panel_message_id.is_empty() || seen.contains(&a.panel_message_id) {
                continue;
            }
            seen.push(a.panel_message_id.clone());
            if let Ok(mid) = a.panel_message_id.parse::<u64>() {
                let _ = channel.delete_message(&ctx.http, MessageId::new(mid)).await;
            }
        }
    }
}

/// Post each channel's panel if it isn't already up. For a shared channel this
/// also reconciles leftover separate/duplicate panels down to a single message.
pub async fn ensure_application_panels(ctx: &Context, guild_id: GuildId) {
    for (channel_id, apps) in apps_by_panel_channel(&guild_id.to_string()) {
        let Ok(raw) = channel_id.parse::<u64>() else { continue };
        let channel = ChannelId::new(raw);

        let mut ids: Vec<String> = Vec::new();
        for a in &apps {
            if !a.panel_message_id.is_empty() && !ids.contains(&a.panel_message_id) {
                ids.push(a.panel_message_id.clone());
            }
        }
        // Split the recorded panels into ones Discord still has and ones it
        // confirms are gone. A lookup that merely failed counts as neither: it
        // leaves the channel alone rather than posting a duplicate on top of a
        // panel that is almost certainly still sitting there.
        let mut live = Vec::new();
        let mut unresolved = false;
        for id in &ids {
            let Ok(raw_mid) = id.parse::<u64>() else { continue };
            let mid = MessageId::new(raw_mid);
            match channel.message(&ctx.http, mid).await {
                Ok(m) => live.push(m),
                Err(e) => {
                    if !crate::common::embeds::is_unknown_message(&e) {
                        eprintln!("⚠️ couldn't check application panel {mid} ({e}); leaving the channel as it is");
                        unresolved = true;
                    }
                }
            }
        }
        if unresolved {
            continue;
        }

        let (name, icon) = super::tickets::guild_meta(ctx, guild_id);
        let (e, rows) = panel_payload(&name, icon, &apps);

        // Already a single shared panel message - refresh it in place.
        if live.len() == 1 && ids.len() == 1 {
            let mut msg = live.remove(0);
            let _ = msg.edit(&ctx.http, EditMessage::new().embed(e).components(rows)).await;
            set_group_panel_message(&guild_id.to_string(), &apps, &msg.id.to_string());
            continue;
        }
        // Otherwise (nothing up yet, or multiple stale panels): clear leftovers
        // and post one fresh panel for the channel.
        for m in &live {
            let _ = m.delete(&ctx.http).await;
        }
        if let Ok(posted) = channel.send_message(&ctx.http, CreateMessage::new().embed(e).components(rows)).await {
            set_group_panel_message(&guild_id.to_string(), &apps, &posted.id.to_string());
            println!(
                "📝 Posted application panel ({}) in #{name}",
                apps.iter().map(|a| a.label.clone()).collect::<Vec<_>>().join(", ")
            );
        }
    }
}

async fn reply_ephemeral(ctx: &Context, i: &ComponentInteraction, content: &str) {
    let _ = i
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content).ephemeral(true)),
        )
        .await;
}

/// Someone picked an application from the panel dropdown.
///
/// Hands straight to the same flow the single-application button uses, so a
/// panel with one app and a panel with six behave identically from here on.
pub async fn handle_app_pick(ctx: &Context, i: &ComponentInteraction) {
    use serenity::model::application::ComponentInteractionDataKind;
    let ComponentInteractionDataKind::StringSelect { values } = &i.data.kind else { return };
    let Some(key) = values.first() else { return };
    let key = key.to_string();
    start_application(ctx, i, &key).await;
}

pub async fn handle_app_apply(ctx: &Context, i: &ComponentInteraction) {
    let key = i.data.custom_id.trim_start_matches("app_apply_").to_string();
    start_application(ctx, i, &key).await;
}

/// The apply flow, entered either from a single application's button or from
/// the dropdown on a combined panel.
async fn start_application(ctx: &Context, i: &ComponentInteraction, key: &str) {
    let Some(guild_id) = i.guild_id else { return };
    let key = key.to_string();
    let Some(app) = get_application(&guild_id.to_string(), &key) else {
        return reply_ephemeral(ctx, i, "Sorry, that application isn't around anymore.").await;
    };
    // Re-check even though the button is disabled when closed - the panel
    // message could be stale, so never let a closed application start.
    if app.closed {
        refresh_app_panel(ctx, guild_id, &app).await;
        return reply_ephemeral(
            ctx,
            i,
            &format!("**{}** applications are closed right now. Do check back soon!", app.label),
        )
        .await;
    }
    if app.review_channel_id.is_empty() {
        return reply_ephemeral(ctx, i, "This application isn't quite ready yet. Please give an admin a heads up.").await;
    }
    if app.questions.is_empty() {
        return reply_ephemeral(ctx, i, "This application doesn't have any questions set up yet. Please let an admin know.").await;
    }
    if active_lock().contains(&i.user.id.get()) {
        return reply_ephemeral(
            ctx,
            i,
            "You've already got an application open in your DMs. Finish that one first, or hit **Cancel Application** there, then come back.",
        )
        .await;
    }

    // Open a DM and send the intro BEFORE acknowledging, so a closed-DM user
    // gets a clear message instead of silently starting an unseen interview.
    let Ok(dm) = i.user.create_dm_channel(&ctx.http).await else {
        return reply_ephemeral(
            ctx,
            i,
            "I couldn't slide into your DMs. Turn on direct messages for this server (Privacy Settings → Allow direct messages from server members), then give Apply another tap.",
        )
        .await;
    };
    let intro = dm
        .id
        .send_message(
            &ctx.http,
            CreateMessage::new().embed(
                CreateEmbed::new()
                    .color(APPY_GREEN)
                    .title("Application Started")
                    .description("Just answer the questions below by sending a message to the bot. Take your time, and be honest."),
            ),
        )
        .await;
    let Ok(intro) = intro else {
        return reply_ephemeral(
            ctx,
            i,
            "I couldn't slide into your DMs. Turn on direct messages for this server (Privacy Settings → Allow direct messages from server members), then give Apply another tap.",
        )
        .await;
    };

    // Appy-style ephemeral confirmation with a Jump-to-application link.
    let jump = CreateActionRow::Buttons(vec![CreateButton::new_link(format!(
        "https://discord.com/channels/@me/{}/{}",
        dm.id, intro.id
    ))
    .label("Jump to application")]);
    let _ = i
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .components(vec![jump])
                    .embed(
                        CreateEmbed::new()
                            .color(APPY_GREEN)
                            .title("Application started")
                            .description("Your application's up and waiting in your DMs. Hit the button below to jump straight to it."),
                    ),
            ),
        )
        .await;

    let ctx2 = ctx.clone();
    let user = i.user.clone();
    tokio::spawn(async move {
        run_dm_application(&ctx2, guild_id, user, app, dm.id).await;
    });
}

async fn dm_notice(ctx: &Context, dm: ChannelId, color: u32, title: &str, description: String) {
    let _ = dm
        .send_message(&ctx.http, CreateMessage::new().embed(CreateEmbed::new().color(color).title(title).description(description)))
        .await;
}

/// Walk the applicant through the questions in DMs, one at a time.
async fn run_dm_application(ctx: &Context, guild_id: GuildId, user: User, app: Application, dm: ChannelId) {
    active_lock().insert(user.id.get());
    let started_at = now_ms();
    // Guard so the "one interview at a time" slot is always released.
    let _guard = ActiveGuard(user.id.get());

    let cap = app_answer_cap(app.questions.len());
    let total = app.questions.len();
    let mut answers: Vec<String> = Vec::new();

    for (idx, question) in app.questions.iter().enumerate() {
        let cancel_row = CreateActionRow::Buttons(vec![CreateButton::new("app_cancel")
            .label("Cancel Application")
            .style(ButtonStyle::Danger)]);
        let q_msg = dm
            .send_message(
                &ctx.http,
                CreateMessage::new()
                    .embed(
                        CreateEmbed::new()
                            .color(APPY_BLURPLE)
                            .title(format!("{} Application", app.label))
                            .description(format!(
                                "{}/{total}. {question}\n\n-# To answer this one, just send your response as a message here.",
                                idx + 1
                            )),
                    )
                    .components(vec![cancel_row]),
            )
            .await
            .ok();

        let q_msg_id = q_msg.as_ref().map(|m| m.id);
        let user_id = user.id;

        // Whichever comes first: the applicant's reply, or a Cancel click.
        let reply_fut = MessageCollector::new(&ctx.shard)
            .author_id(user_id)
            .channel_id(dm)
            .timeout(APP_QUESTION_TIMEOUT)
            .next();
        let cancel_fut = ComponentInteractionCollector::new(&ctx.shard)
            .timeout(APP_QUESTION_TIMEOUT)
            .filter(move |i| {
                i.user.id == user_id && i.data.custom_id == "app_cancel" && Some(i.message.id) == q_msg_id
            })
            .next();

        enum Outcome {
            Reply(Box<serenity::model::channel::Message>),
            Cancel,
            Timeout,
        }
        let outcome = tokio::select! {
            m = reply_fut => match m { Some(m) => Outcome::Reply(Box::new(m)), None => Outcome::Timeout },
            c = cancel_fut => match c {
                Some(c) => { let _ = c.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await; Outcome::Cancel }
                None => Outcome::Timeout,
            },
        };

        // Retire the Cancel button for this question.
        if let Some(mut m) = q_msg {
            let _ = m.edit(&ctx.http, EditMessage::new().components(vec![])).await;
        }

        match outcome {
            Outcome::Cancel => {
                dm_notice(ctx, dm, APPY_RED, "Application cancelled", format!(
                    "All good, I've scrapped your {} application. Nothing got sent. Swing by the panel whenever you want to give it another go.",
                    app.label
                )).await;
                return;
            }
            Outcome::Timeout => {
                dm_notice(ctx, dm, APPY_RED, "Application cancelled", format!(
                    "Looks like you wandered off, so I've closed out your {} application for now. Start fresh from the panel whenever you're ready.",
                    app.label
                )).await;
                return;
            }
            Outcome::Reply(msg) => {
                let mut content = msg.content.trim().to_string();
                if content.to_lowercase() == "cancel" {
                    dm_notice(ctx, dm, APPY_RED, "Application cancelled", format!(
                        "All good, I've scrapped your {} application. Nothing got sent.",
                        app.label
                    )).await;
                    return;
                }
                // Image/file-only answer.
                if content.is_empty() && !msg.attachments.is_empty() {
                    content = msg.attachments.iter().map(|a| a.url.clone()).collect::<Vec<_>>().join("\n");
                }
                answers.push(if content.is_empty() { "*(left blank)*".to_string() } else { truncate(&content, cap) });
            }
        }
    }

    // The application could have been closed or deleted mid-interview.
    let fresh = get_application(&guild_id.to_string(), &app.key);
    let still_open = fresh.as_ref().map(|f| !f.closed && !f.review_channel_id.is_empty()).unwrap_or(false);
    if !still_open {
        dm_notice(ctx, dm, APPY_RED, "Applications closed", format!(
            "Ah, {} applications shut just as you were wrapping up, so this one didn't make it through. Sorry about the timing - catch it next time they open.",
            app.label
        )).await;
        return;
    }

    let ok = finalize_application(ctx, guild_id, &user, &fresh.unwrap(), &answers, started_at).await;
    if ok {
        dm_notice(ctx, dm, APPY_GREEN, "Application submitted",
            "Your application has been submitted.\n\nThe team will give it a read and get back to you right here. Thanks for taking the time, and good luck!".to_string()).await;
    } else {
        dm_notice(ctx, dm, APPY_RED, "Something went wrong",
            "Something broke on my end and your application didn't go through. Give a staff member a nudge and they'll get it sorted.".to_string()).await;
    }
}

/// Releases the "one in-progress interview per user" slot on every exit path.
struct ActiveGuard(u64);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        active_lock().remove(&self.0);
    }
}

/// Post a completed application to its review channel. Returns true on success.
async fn finalize_application(
    ctx: &Context,
    guild_id: GuildId,
    user: &User,
    app: &Application,
    answers: &[String],
    started_at: i64,
) -> bool {
    let Ok(raw) = app.review_channel_id.parse::<u64>() else { return false };
    let review_channel = ChannelId::new(raw);

    let member = fetch_member(ctx, guild_id, user.id).await;
    let duration_sec = ((now_ms() - started_at) as f64 / 1000.0).round().max(0.0) as i64;
    let joined_unix = member.as_ref().and_then(|m| m.joined_at).map(|t| t.unix_timestamp());
    let submitted_unix = now_ms() / 1000;

    let mut stats = vec![
        format!("UserId: `{}`", user.id),
        format!("Username: `{}`", user.name),
        format!("User: <@{}>", user.id),
        format!("Duration: `{duration_sec}s`"),
    ];
    if let Some(j) = joined_unix {
        stats.push(format!("Joined guild <t:{j}:R>"));
    }
    stats.push(format!("Submitted <t:{submitted_unix}:R>"));

    let mut e = CreateEmbed::new()
        .color(APP_PENDING)
        .title(truncate(&format!("{}'s '{} Application' Application Submitted", user.name, app.label), 256))
        .thumbnail(user.face())
        .timestamp(Timestamp::now());
    for (idx, q) in app.questions.iter().enumerate() {
        let value = answers.get(idx).cloned().unwrap_or_else(|| "*(left blank)*".to_string());
        e = e.field(truncate(&format!("{}. {q}", idx + 1), 256), truncate(&value, 1024), false);
    }
    e = e.field("Submission stats", truncate(&stats.join("\n"), 1024), false);

    let row1 = CreateActionRow::Buttons(vec![
        CreateButton::new(format!("app_accept_{}_{}", app.key, user.id)).label("Accept").style(ButtonStyle::Success),
        CreateButton::new(format!("app_deny_{}_{}", app.key, user.id)).label("Deny").style(ButtonStyle::Danger),
        CreateButton::new(format!("app_acceptwithreason_{}_{}", app.key, user.id))
            .label("Accept with reason")
            .style(ButtonStyle::Success),
    ]);
    let row2 = CreateActionRow::Buttons(vec![CreateButton::new(format!("app_denywithreason_{}_{}", app.key, user.id))
        .label("Deny with reason")
        .style(ButtonStyle::Danger)]);

    let posted = review_channel
        .send_message(&ctx.http, CreateMessage::new().embed(e).components(vec![row1, row2]))
        .await;
    if posted.is_err() {
        return false;
    }
    sec_log(
        ctx,
        guild_id,
        "New Application",
        &format!(
            "<@{}> just applied for **{}**. It's waiting for a look in <#{}>.",
            user.id, app.label, review_channel
        ),
        colors::INFO,
    )
    .await;
    true
}

/// Parse `app_accept_<key>_<userId>` → (key, userId).
fn parse_review_custom_id(custom_id: &str, prefix: &str) -> (String, String) {
    let rest = custom_id.trim_start_matches(prefix);
    match rest.rfind('_') {
        Some(i) => (rest[..i].to_string(), rest[i + 1..].to_string()),
        None => (rest.to_string(), String::new()),
    }
}

/// Apps where existing members - anyone already holding one of the app's own
/// accepted (whitelist) roles - can review pending applications for that same
/// app, on top of staff holding the mod role. Police + crime families only;
/// staff applications still require the mod role.
const PEER_REVIEW_APP_KEYS: [&str; 3] = ["nypd", "gambino", "colombo"];

/// A configured channel id, or nothing when it was left blank.
fn channel_from(raw: &str) -> Option<ChannelId> {
    raw.parse::<u64>().ok().map(ChannelId::new)
}

fn can_review_app(member: &serenity::model::guild::Member, owner_id: UserId, app: &Application) -> bool {
    if is_mod(member, owner_id) {
        return true;
    }
    if !PEER_REVIEW_APP_KEYS.contains(&app.key.as_str()) {
        return false;
    }
    app.accepted_role_ids.iter().any(|id| member.roles.iter().any(|r| r.to_string() == *id))
}

const NOT_ALLOWED: &str = "Only staff, or a whitelisted member of this app, can review applications.";
const GONE: &str = "That application type doesn't exist anymore.";

/// Shared accept path for both the plain "Accept" button and the "Accept with
/// reason" modal submit - grants roles, repaints the review message green with
/// every button retired, then DMs the applicant.
#[allow(clippy::too_many_arguments)]
async fn perform_app_accept(
    ctx: &Context,
    guild_id: GuildId,
    channel_id: ChannelId,
    actor: &User,
    key: &str,
    user_id: &str,
    reason: Option<&str>,
    message_id: MessageId,
) {
    let Some(app) = get_application(&guild_id.to_string(), key) else { return };
    let Ok(uid_raw) = user_id.parse::<u64>() else { return };
    let applicant = fetch_member(ctx, guild_id, UserId::new(uid_raw)).await;

    let mut granted = 0usize;
    let mut failed: Vec<String> = Vec::new();
    if let Some(m) = &applicant {
        let info = crate::common::guildinfo::GuildInfo::from_cache(ctx, guild_id);
        for role_id in &app.accepted_role_ids {
            let Ok(rid) = role_id.parse::<u64>() else {
                failed.push(format!("`{role_id}` (missing)"));
                continue;
            };
            let role = RoleId::new(rid);
            match &info {
                Some(i) if !i.roles.contains_key(&role) => {
                    failed.push(format!("`{role_id}` (missing)"));
                    continue;
                }
                Some(i) if !i.role_editable(role) => {
                    failed.push(format!("{} (above me)", i.role_name(role)));
                    continue;
                }
                _ => {}
            }
            if m.add_role(&ctx.http, role).await.is_ok() {
                granted += 1;
            } else {
                failed.push(info.as_ref().map(|i| i.role_name(role)).unwrap_or_else(|| role_id.clone()));
            }
        }
    }

    repaint_review(ctx, channel_id, message_id, APPY_GREEN, None, "app_done_accept", &format!("Accepted by {}", actor.name), ButtonStyle::Success, '✅', channel_from(&app.accepted_channel_id)).await;

    if let Some(m) = &applicant {
        let _ = m
            .user
            .direct_message(
                &ctx.http,
                CreateMessage::new().embed(CreateEmbed::new().color(APPY_GREEN).title("Application accepted").description(
                    format!(
                        "Your application for `{} Application` has been accepted by <@{}>.{}",
                        app.label,
                        actor.id,
                        reason.map(|r| format!("\n\nReason: {r}")).unwrap_or_default()
                    ),
                )),
            )
            .await;
    }

    sec_log(
        ctx,
        guild_id,
        "Application Accepted",
        &format!(
            "<@{}> accepted <@{user_id}>'s **{}** application and handed them **{granted}** role{}.{}{}{}",
            actor.id,
            app.label,
            if granted == 1 { "" } else { "s" },
            reason.map(|r| format!("\nReason given: {r}")).unwrap_or_default(),
            if failed.is_empty() { String::new() } else { format!("\nHeads up, I couldn't grant: {}", failed.join(", ")) },
            if applicant.is_none() { "\nThey've since left the server, so no roles were applied." } else { "" }
        ),
        colors::SUCCESS,
    )
    .await;
}

/// Shared deny path - repaints the review message red with every button
/// retired, then DMs the applicant.
#[allow(clippy::too_many_arguments)]
async fn perform_app_deny(
    ctx: &Context,
    guild_id: GuildId,
    channel_id: ChannelId,
    actor: &User,
    key: &str,
    user_id: &str,
    reason: Option<&str>,
    message_id: MessageId,
) {
    let app = get_application(&guild_id.to_string(), key);
    let label = app.as_ref().map(|a| a.label.clone()).unwrap_or_else(|| "that role".to_string());

    repaint_review(ctx, channel_id, message_id, APPY_RED, reason, "app_done_deny", &format!("Denied by {}", actor.name), ButtonStyle::Danger, '⛔', app.as_ref().and_then(|a| channel_from(&a.denied_channel_id))).await;

    if let Ok(uid_raw) = user_id.parse::<u64>() {
        if let Some(m) = fetch_member(ctx, guild_id, UserId::new(uid_raw)).await {
            let _ = m
                .user
                .direct_message(
                    &ctx.http,
                    CreateMessage::new().embed(CreateEmbed::new().color(APPY_RED).title("Application denied").description(
                        format!(
                            "Your application for `{label} Application` has been denied by <@{}>.{}",
                            actor.id,
                            reason.map(|r| format!("\n\nReason: {r}")).unwrap_or_default()
                        ),
                    )),
                )
                .await;
        }
    }

    sec_log(
        ctx,
        guild_id,
        "Application Denied",
        &format!(
            "<@{}> turned down <@{user_id}>'s **{}** application.{}",
            actor.id,
            app.as_ref().map(|a| a.label.clone()).unwrap_or_else(|| key.to_string()),
            reason.map(|r| format!(" Reason given: {r}")).unwrap_or_default()
        ),
        colors::DANGER,
    )
    .await;
}

/// Recolour the review embed, optionally append a Reason field, and replace
/// every control with a single disabled "done" button.
#[allow(clippy::too_many_arguments)]
/// Recolour the review message and retire its buttons, and optionally file a
/// copy of the finished application in an outcome channel.
///
/// The copy is built from the same rebuilt embed, so what lands in the
/// accepted or denied channel is exactly what the reviewer saw, minus the
/// buttons, rather than a summary that could drift from it.
async fn repaint_review(
    ctx: &Context,
    channel_id: ChannelId,
    message_id: MessageId,
    color: u32,
    reason: Option<&str>,
    done_id: &str,
    done_label: &str,
    style: ButtonStyle,
    emoji: char,
    file_to: Option<ChannelId>,
) {
    let Ok(mut msg) = channel_id.message(&ctx.http, message_id).await else { return };
    let Some(old) = msg.embeds.first().cloned() else { return };

    let mut e = CreateEmbed::new().color(color);
    if let Some(t) = old.title {
        e = e.title(t);
    }
    if let Some(d) = old.description {
        e = e.description(d);
    }
    if let Some(thumb) = old.thumbnail {
        e = e.thumbnail(thumb.url);
    }
    for f in &old.fields {
        e = e.field(f.name.clone(), f.value.clone(), f.inline);
    }
    if let Some(r) = reason {
        e = e.field("Reason", truncate(r, 1024), false);
    }
    e = e.timestamp(Timestamp::now());

    if let Some(dest) = file_to {
        let _ = dest.send_message(&ctx.http, CreateMessage::new().embed(e.clone())).await;
    }

    let done = CreateActionRow::Buttons(vec![CreateButton::new(done_id)
        .label(truncate(done_label, 80))
        .emoji(emoji)
        .style(style)
        .disabled(true)]);
    let _ = msg.edit(&ctx.http, EditMessage::new().embed(e).components(vec![done])).await;
}

/// Resolve the reviewer + app and check permission, shared by all four buttons.
async fn review_guard(
    ctx: &Context,
    i: &ComponentInteraction,
    prefix: &str,
) -> Option<(GuildId, String, String, Application)> {
    let guild_id = i.guild_id?;
    let (key, user_id) = parse_review_custom_id(&i.data.custom_id, prefix);
    let Some(app) = get_application(&guild_id.to_string(), &key) else {
        reply_ephemeral(ctx, i, GONE).await;
        return None;
    };
    let owner_id = ctx.cache.guild(guild_id).map(|g| g.owner_id)?;
    let member = fetch_member(ctx, guild_id, i.user.id).await?;
    if !can_review_app(&member, owner_id, &app) {
        reply_ephemeral(ctx, i, NOT_ALLOWED).await;
        return None;
    }
    Some((guild_id, key, user_id, app))
}

pub async fn handle_app_accept(ctx: &Context, i: &ComponentInteraction) {
    let Some((guild_id, key, user_id, _)) = review_guard(ctx, i, "app_accept_").await else { return };
    let _ = i.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
    perform_app_accept(ctx, guild_id, i.channel_id, &i.user, &key, &user_id, None, i.message.id).await;
}

pub async fn handle_app_deny(ctx: &Context, i: &ComponentInteraction) {
    let Some((guild_id, key, user_id, _)) = review_guard(ctx, i, "app_deny_").await else { return };
    let _ = i.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
    perform_app_deny(ctx, guild_id, i.channel_id, &i.user, &key, &user_id, None, i.message.id).await;
}

async fn show_reason_modal(ctx: &Context, i: &ComponentInteraction, modal_id: String, title: String) {
    let modal = CreateModal::new(modal_id, truncate(&title, 45)).components(vec![CreateActionRow::InputText(
        CreateInputText::new(InputTextStyle::Paragraph, "Reason (optional, shared with them)", "reason")
            .required(false)
            .max_length(500),
    )]);
    let _ = i.create_response(&ctx.http, CreateInteractionResponse::Modal(modal)).await;
}

pub async fn handle_app_accept_with_reason(ctx: &Context, i: &ComponentInteraction) {
    let Some((_, key, user_id, app)) = review_guard(ctx, i, "app_acceptwithreason_").await else { return };
    show_reason_modal(
        ctx,
        i,
        format!("app_acceptreason_{key}_{user_id}_{}", i.message.id),
        format!("Accept {} Application", app.label),
    )
    .await;
}

pub async fn handle_app_deny_with_reason(ctx: &Context, i: &ComponentInteraction) {
    let Some((_, key, user_id, app)) = review_guard(ctx, i, "app_denywithreason_").await else { return };
    show_reason_modal(
        ctx,
        i,
        format!("app_denyreason_{key}_{user_id}_{}", i.message.id),
        format!("Deny {} Application", app.label),
    )
    .await;
}

/// custom_id: `app_(accept|deny)reason_<key>_<userId>_<messageId>`
fn parse_reason_modal_id(custom_id: &str, prefix: &str) -> Option<(String, String, MessageId)> {
    let rest = custom_id.trim_start_matches(prefix);
    let mut parts: Vec<&str> = rest.split('_').collect();
    let message_id = parts.pop()?.parse::<u64>().ok().map(MessageId::new)?;
    let user_id = parts.pop()?.to_string();
    Some((parts.join("_"), user_id, message_id))
}

pub async fn handle_app_reason_modal(ctx: &Context, i: &ModalInteraction, accept: bool) {
    let Some(guild_id) = i.guild_id else { return };
    let prefix = if accept { "app_acceptreason_" } else { "app_denyreason_" };
    let Some((key, user_id, message_id)) = parse_reason_modal_id(&i.data.custom_id, prefix) else { return };

    let reason = i
        .data
        .components
        .iter()
        .flat_map(|row| row.components.iter())
        .find_map(|c| match c {
            serenity::model::application::ActionRowComponent::InputText(it) => it.value.clone(),
            _ => None,
        })
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    let _ = i.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
    if accept {
        perform_app_accept(ctx, guild_id, i.channel_id, &i.user, &key, &user_id, reason.as_deref(), message_id).await;
    } else {
        perform_app_deny(ctx, guild_id, i.channel_id, &i.user, &key, &user_id, reason.as_deref(), message_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(key: &str, label: &str, closed: bool, questions: usize) -> Application {
        Application {
            key: key.into(),
            label: label.into(),
            emoji: "📝".into(),
            questions: (0..questions).map(|n| format!("Question {n}")).collect(),
            closed,
            ..Default::default()
        }
    }

    /// Serialising the row is the only way to see what a serenity builder
    /// actually produces without a live gateway.
    fn row_json(apps: &[Application]) -> String {
        let (_, rows) = panel_payload("Test Server", None, apps);
        serde_json::to_string(&rows).unwrap()
    }

    /// One application keeps its single Apply button: a dropdown to choose
    /// between one thing would be worse, not better.
    #[test]
    fn a_lone_application_still_gets_a_button() {
        let json = row_json(&[app("staff", "Staff", false, 6)]);
        assert!(json.contains("app_apply_staff"), "expected an apply button: {json}");
        assert!(!json.contains("app_pick"), "one application should not get a chooser");
    }

    /// Several applications on one panel collapse into a single chooser rather
    /// than a row of buttons.
    #[test]
    fn several_applications_share_one_chooser() {
        let apps = [
            app("staff", "Staff", false, 6),
            app("nypd", "NYPD", false, 14),
            app("gambino", "Gambino", false, 7),
            app("colombo", "Colombo", false, 7),
        ];
        let json = row_json(&apps);
        assert!(json.contains("app_pick"), "expected one chooser: {json}");
        for key in ["staff", "nypd", "gambino", "colombo"] {
            assert!(json.contains(key), "{key} should be an option: {json}");
        }
        assert!(!json.contains("app_apply_"), "the chooser replaces the per-app buttons");
    }

    /// A closed application stays visible and labelled, so people can see it
    /// exists rather than assuming it was removed.
    #[test]
    fn a_closed_application_is_shown_as_closed_not_hidden() {
        let apps = [app("staff", "Staff", true, 6), app("nypd", "NYPD", false, 14)];
        let json = row_json(&apps);
        assert!(json.contains("Staff (closed)"), "closed apps stay listed: {json}");
        assert!(json.contains("NYPD"));
    }

    /// With nothing open there is nothing to choose, so the menu says so
    /// instead of opening onto a list that cannot be used.
    #[test]
    fn a_panel_with_everything_closed_disables_the_chooser() {
        let apps = [app("staff", "Staff", true, 6), app("nypd", "NYPD", true, 14)];
        let json = row_json(&apps);
        assert!(json.contains("\"disabled\":true"), "chooser should be disabled: {json}");
        assert!(json.contains("closed right now"), "and should say why: {json}");
    }

    /// Discord refuses a select menu with more than 25 options.
    #[test]
    fn the_chooser_stays_within_discords_option_limit() {
        let apps: Vec<Application> =
            (0..40).map(|n| app(&format!("k{n}"), &format!("App {n}"), false, 3)).collect();
        let json = row_json(&apps);
        assert_eq!(json.matches("\"value\":").count(), 25, "must cap at 25 options");
    }

    /// Question 7 names the faction without an article. Building it from the
    /// same string as "join the NCR" produced "a higher-ranking the NCR
    /// member", which is the sort of thing only reading the output catches.
    #[test]
    fn faction_questions_read_correctly_in_both_positions() {
        use crate::state::applications::faction_questions;
        let q = faction_questions("the NCR", "NCR", "an NCR soldier", "Situational?");
        assert_eq!(q.len(), 7);
        assert!(q[2].contains("join the NCR"), "q3 takes the article: {}", q[2]);
        assert!(
            q[6].contains("higher-ranking NCR member"),
            "q7 must not carry the article: {}",
            q[6]
        );
        assert!(!q[6].contains("the NCR member"), "article leaked into q7: {}", q[6]);
    }
}
