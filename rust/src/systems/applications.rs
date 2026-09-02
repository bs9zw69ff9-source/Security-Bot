//! Application System (Appy-style DM interview → staff review → role grant).

use once_cell::sync::Lazy;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, CreateModal, EditMessage};
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

/// The review embed's fixed costs: the title and the submission-stats field.
/// Both are truncated to these lengths where they are built, so the budget
/// below can rely on them.
const REVIEW_TITLE_CAP: usize = 200;
const REVIEW_STATS_CAP: usize = 400;
const REVIEW_OVERHEAD: usize = REVIEW_TITLE_CAP + REVIEW_STATS_CAP + 16;

/// Per-answer character cap, so the finished review embed stays under
/// Discord's 6000-character total and the 1024-per-field limit.
///
/// The questions count towards that total too, since each one is a field name.
/// A flat share of the budget was fine while applications were short, but
/// fourteen long questions carry roughly 900 characters of their own, and the
/// answers were still being allowed the full 5200 on top. Discord rejects the
/// whole embed when the total goes over, so the applicant answers everything
/// and then gets told something broke. The budget is now what's left once the
/// questions and the fixed costs are paid for.
fn app_answer_cap(questions: &[String]) -> usize {
    let count = questions.len().max(1);
    // Each field name is rendered as "N. <question>".
    let names: usize = questions.iter().map(|q| q.chars().count() + 4).sum();
    // 5800 rather than 6000, leaving a little room for miscounts.
    let budget = 5800usize.saturating_sub(names + REVIEW_OVERHEAD);
    (budget / count).clamp(200, 1024)
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
    // Deliberately plain: label only, matching the ticket panel's buttons.
    // A closed application stays on the panel, greyed out and labelled, so
    // people can see it exists rather than assuming it was removed.
    let label = if app.closed {
        truncate(&format!("{} (closed)", app.label), 80)
    } else {
        truncate(&app.label, 80)
    };
    CreateButton::new(format!("app_apply_{}", app.key))
        .label(label)
        .style(ButtonStyle::Secondary)
        .disabled(app.closed)
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

fn panel_payload(guild_name: &str, icon: Option<String>, apps: &[Application]) -> (CreateEmbed, Vec<CreateActionRow>) {
    if apps.len() == 1 {
        return (
            build_app_panel_embed(guild_name, icon, &apps[0]),
            vec![CreateActionRow::Buttons(vec![build_apply_button(&apps[0])])],
        );
    }
    // Same layout as the ticket panel: one button per application, wrapped
    // into rows of five, which is Discord's limit per row.
    let buttons: Vec<CreateButton> = apps.iter().take(25).map(build_apply_button).collect();
    let rows = buttons.chunks(5).map(|c| CreateActionRow::Buttons(c.to_vec())).collect();
    (build_combined_panel_embed(guild_name, icon, apps), rows)
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

    // The panel channel has to belong to the guild whose applications these
    // are. Discord posts by channel id and does not check, so a channel id
    // from another server posts there quite happily: the panel appears in the
    // wrong place, wears the wrong server's name in its footer, and every
    // button is dead, because the click arrives from a guild that has no such
    // application configured. Refusing here is what turns that into one clear
    // log line instead of a panel that looks fine and does nothing.
    let belongs = ctx.cache.guild(guild_id).map(|g| g.channels.contains_key(&channel)).unwrap_or(false);
    if !belongs {
        eprintln!(
            "⚠️ not posting the {} panel: channel {channel} is not in guild {guild_id}. \
             The application is configured under the wrong server, so the buttons would not work.",
            apps.iter().map(|a| a.key.clone()).collect::<Vec<_>>().join(", ")
        );
        return;
    }

    // Same check the ticket panel does, so a permissions problem says which
    // permission rather than leaving a panel quietly missing.
    let missing = crate::common::embeds::missing_panel_permissions(ctx, guild_id, channel);
    if !missing.is_empty() {
        eprintln!(
            "⚠️ can't post the {} panel in {channel}: I'm missing {}.",
            apps.iter().map(|a| a.key.clone()).collect::<Vec<_>>().join(", "),
            missing.join(", ")
        );
        return;
    }

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
    match channel.send_message(&ctx.http, CreateMessage::new().embed(e).components(rows)).await {
        Ok(posted) => set_group_panel_message(&guild_id.to_string(), apps, &posted.id.to_string()),
        Err(e) => eprintln!("⚠️ couldn't post the application panel in {channel}: {e}"),
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

/// Clear earlier application panels in each panel channel, keeping the one
/// currently tracked for that channel. Run at boot, after panels are up.
pub async fn sweep_duplicate_application_panels(ctx: &Context, guild_id: GuildId) {
    for (channel_id, apps) in apps_by_panel_channel(&guild_id.to_string()) {
        let Ok(raw) = channel_id.parse::<u64>() else { continue };
        let keep = apps
            .iter()
            .find(|a| !a.panel_message_id.is_empty())
            .and_then(|a| a.panel_message_id.parse::<u64>().ok())
            .map(MessageId::new);
        // Covers both the current buttons and any panel still on the old
        // dropdown, since either is ours and either would be a duplicate.
        for marker in ["app_apply_", "app_pick"] {
            crate::common::embeds::remove_duplicate_panels(ctx, ChannelId::new(raw), keep, marker, "application").await;
        }
    }
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

/// Fallback for a panel that still carries the old dropdown.
///
/// Panels are edited in place on boot, so they become buttons on the next
/// restart. If that edit ever fails, this keeps the stale dropdown working
/// instead of leaving a dead control on the panel.
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
        // Naming the key matters: this fires when a panel is still up but the
        // application behind it is not in config, which usually means a write
        // to guardian.db failed and the seed was lost on the next restart.
        // Without the key in the message there is nothing to go on.
        eprintln!(
            "⚠️ apply clicked for `{key}` in guild {guild_id}, but no such application is configured. \
             Known keys: [{}]. If that list is missing something it should have, check for earlier \
             'db write applications' errors and that guardian.db is writable by the bot's user.",
            get_applications(&guild_id.to_string()).keys().cloned().collect::<Vec<_>>().join(", ")
        );
        return reply_ephemeral(
            ctx,
            i,
            &format!("I can't find the **{key}** application any more. This panel is out of date, so give a staff member a nudge."),
        )
        .await;
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

    let cap = app_answer_cap(&app.questions);
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
        .title(truncate(&format!("{}'s '{} Application' Application Submitted", user.name, app.label), REVIEW_TITLE_CAP))
        .thumbnail(user.face())
        .timestamp(Timestamp::now());
    for (idx, q) in app.questions.iter().enumerate() {
        let value = answers.get(idx).cloned().unwrap_or_else(|| "*(left blank)*".to_string());
        e = e.field(truncate(&format!("{}. {q}", idx + 1), 256), truncate(&value, 1024), false);
    }
    e = e.field("Submission stats", truncate(&stats.join("\n"), REVIEW_STATS_CAP), false);

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

/// A configured channel id, or nothing when it was left blank.
fn channel_from(raw: &str) -> Option<ChannelId> {
    raw.parse::<u64>().ok().map(ChannelId::new)
}

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
    // The outcome lived on the retired button, which is not copied anywhere.
    // Now that a filed application leaves the pending channel entirely, it has
    // to be in the embed or it is lost.
    e = e.field("Outcome", truncate(done_label, 1024), false).timestamp(Timestamp::now());

    // File it, then take it out of pending, so the pending channel only ever
    // holds what still needs a decision.
    //
    // Only ever delete once the copy is safely posted. If there is no outcome
    // channel set, or posting to it fails, the application stays where it is,
    // marked and with its buttons retired: a decided application sitting in
    // the wrong channel is a great deal better than one that is simply gone.
    if let Some(dest) = file_to {
        match dest.send_message(&ctx.http, CreateMessage::new().embed(e.clone())).await {
            Ok(_) => {
                if msg.delete(&ctx.http).await.is_ok() {
                    return;
                }
                eprintln!(
                    "⚠️ filed the application to {dest} but couldn't remove it from {channel_id}; \
                     it will be marked in place instead so it isn't there twice unmarked."
                );
            }
            Err(err) => eprintln!(
                "⚠️ couldn't file the decided application in {dest} ({err}); leaving it in {channel_id} instead of deleting it."
            ),
        }
    }

    let done = CreateActionRow::Buttons(vec![CreateButton::new(done_id)
        .label(truncate(done_label, 80))
        .emoji(emoji)
        .style(style)
        .disabled(true)]);
    let _ = msg.edit(&ctx.http, EditMessage::new().embed(e).components(vec![done])).await;
}

/// Resolve the reviewer + app, shared by all four buttons.
///
/// There is no permission check here on purpose: whoever can see the review
/// channel can review. The channel's own permissions are the gate, which is
/// where it belongs, since that is the thing a server actually configures. The
/// role check that used to live here also meant a reviewer had to be resolvable
/// through the cache or an HTTP fetch, and when that came back empty the button
/// just did nothing at all with no message.
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

    /// Worst case for an application: every answer runs right up to the cap.
    /// Discord counts the questions, the answers, the title and the stats
    /// field against one 6000-character budget, and rejects the message if the
    /// total goes over, which loses the whole application.
    fn worst_case_embed_size(questions: &[String]) -> usize {
        let names: usize = questions.iter().map(|q| q.chars().count() + 4).sum();
        names + app_answer_cap(questions) * questions.len() + REVIEW_OVERHEAD
    }

    /// The staff application the wasteland server uses is the longest one the
    /// bot carries, so it is the one that would overflow first.
    #[test]
    fn a_long_application_still_fits_in_one_embed() {
        let questions = crate::state::applications::wasteland_staff_questions();
        assert_eq!(questions.len(), 14);
        let total = worst_case_embed_size(&questions);
        assert!(total <= 6000, "a full staff application would be {total} characters, over Discord's 6000");
        // Still worth answering: a cap this low would make the form useless.
        let cap = app_answer_cap(&questions);
        assert!(cap >= 250, "answers capped at {cap}, too short to be usable");
    }

    /// A short application should not be squeezed by the same accounting.
    #[test]
    fn a_short_application_keeps_a_generous_cap() {
        let questions: Vec<String> = (0..6).map(|n| format!("Question {n}")).collect();
        assert!(app_answer_cap(&questions) > 800);
        assert!(worst_case_embed_size(&questions) <= 6000);
    }

    /// One application, one button.
    #[test]
    fn a_lone_application_gets_a_button() {
        let json = row_json(&[app("staff", "Staff", false, 6)]);
        assert!(json.contains("app_apply_staff"), "expected an apply button: {json}");
    }

    /// Several applications get a button each, laid out like the ticket panel
    /// rather than collapsed behind a chooser.
    #[test]
    fn several_applications_each_get_their_own_button() {
        let apps = [
            app("ncr", "NCR", false, 7),
            app("legion", "Caesar's Legion", false, 7),
            app("bos", "Brotherhood of Steel", false, 7),
            app("enclave", "Enclave", false, 7),
        ];
        let json = row_json(&apps);
        for key in ["ncr", "legion", "bos", "enclave"] {
            assert!(json.contains(&format!("app_apply_{key}")), "{key} needs a button: {json}");
        }
        assert!(!json.contains("app_pick"), "no dropdown should be built");
    }

    /// The label is the application name and nothing else: no emoji, no
    /// question count, no "Apply for" prefix.
    #[test]
    fn button_labels_carry_nothing_but_the_name() {
        let json = row_json(&[app("ncr", "NCR", false, 7), app("bos", "Brotherhood of Steel", false, 7)]);
        assert!(json.contains(r#""label":"NCR""#), "plain label expected: {json}");
        assert!(json.contains(r#""label":"Brotherhood of Steel""#), "plain label expected: {json}");
        assert!(!json.contains("Apply for"), "no prefix on the label: {json}");
        assert!(!json.contains("question"), "question counts must not appear: {json}");
        assert!(!json.contains("emoji"), "no emoji on application buttons: {json}");
    }

    /// A closed application stays on the panel, greyed out and labelled, so
    /// people can see it exists rather than assuming it was removed.
    #[test]
    fn a_closed_application_is_shown_disabled_not_hidden() {
        let json = row_json(&[app("staff", "Staff", true, 6), app("nypd", "NYPD", false, 14)]);
        assert!(json.contains("Staff (closed)"), "closed apps stay listed: {json}");
        assert!(json.contains(r#""disabled":true"#), "and cannot be clicked: {json}");
        assert!(json.contains("NYPD"));
    }

    /// Discord allows five buttons per row, so more than that has to wrap.
    #[test]
    fn buttons_wrap_into_rows_of_five() {
        let apps: Vec<Application> =
            (0..12).map(|n| app(&format!("k{n}"), &format!("App {n}"), false, 3)).collect();
        let (_, rows) = panel_payload("Test Server", None, &apps);
        assert_eq!(rows.len(), 3, "12 buttons should be 5 + 5 + 2");
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

    /// When every application on a panel has the same requirements, the block
    /// is shown once rather than repeated under each name. That is what the
    /// four faction applications do, so their panel is one requirements block
    /// and a row of buttons.
    #[test]
    fn a_panel_with_matching_requirements_shows_the_block_once() {
        let apps = [
            app("ncr", "NCR", false, 7),
            app("legion", "Caesar's Legion", false, 7),
            app("bos", "Brotherhood of Steel", false, 7),
            app("enclave", "Enclave", false, 7),
        ];
        let (embed, _) = panel_payload("New Vegas", None, &apps);
        let json = serde_json::to_string(&embed).unwrap();
        assert_eq!(json.matches("REQUIREMENTS").count(), 1, "one block, not four: {json}");
        // Names live on the buttons, so they are not repeated in the body.
        assert!(!json.contains("Caesar"), "app names belong on the buttons: {json}");
    }
}
