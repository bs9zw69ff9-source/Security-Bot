//! Application forms, ticket forms, and the staff review queue.
//!
//! Every write path re-checks access at the moment of the write. A form that
//! was open when it was rendered may have been closed since, and a reviewer may
//! have lost their role between loading the queue and clicking Accept.

use axum::extract::Path;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serenity::model::id::UserId;

use crate::common::config::now_ms;
use crate::state::applications::get_applications;
use crate::state::tickets::get_ticket_config;
use crate::systems::applications::{build_requirements, finalize_application};
use crate::systems::tickets::open_ticket;
use crate::web::routes::{collect_answers, find_app, FormBody};
use crate::web::views::{csrf_field, empty, esc, note_err, note_ok, page};

use crate::web::routes::{guild_access, is_staff, may_review, require_session};

fn html(body: String) -> Response {
    axum::response::Html(body).into_response()
}

// ── applications ──────────────────────────────────────────────

pub async fn apply_form(h: HeaderMap, Path((guild, key)): Path<(String, String)>) -> Response {
    let path = format!("/g/{guild}/apply/{key}");
    let session = match require_session(&h, &path) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if let Err(r) = guild_access(&session, &guild) {
        return *r;
    }
    let Some(app) = find_app(&guild, &key) else {
        return html(page("Not found", Some(&session), &empty("That application doesn't exist.")));
    };
    if app.closed {
        return html(page(
            &app.label,
            Some(&session),
            &format!("<h1>{}</h1>{}", esc(&app.label), note_err("Applications are closed for this one right now.")),
        ));
    }

    let mut fields = String::new();
    for (i, q) in app.questions.iter().enumerate() {
        fields.push_str(&format!(
            r#"<div class="q"><label for="q{i}"><span class="n">{n}.</span>{q}</label>
               <textarea id="q{i}" name="q{i}" rows="3" required></textarea></div>"#,
            i = i,
            n = i + 1,
            q = esc(q)
        ));
    }
    let reqs = build_requirements(&app);
    let req_block = if reqs.trim().is_empty() {
        String::new()
    } else {
        format!(r#"<div class="req">{}</div>"#, esc(&reqs).replace('\n', "<br>"))
    };

    let body = format!(
        r#"<h1 style="margin:34px 0 4px;font-size:26px">{emoji} {label}</h1>
           <p class="muted" style="margin:0 0 22px">Your answers go straight to the server's review channel.</p>
           {req}
           <form method="post" class="card">{csrf}{fields}
             <button class="btn" type="submit">Submit application</button>
           </form>"#,
        emoji = esc(&app.emoji),
        label = esc(&app.label),
        req = req_block,
        csrf = csrf_field(&session),
        fields = fields
    );
    html(page(&app.label, Some(&session), &body))
}

pub async fn apply_submit(
    h: HeaderMap,
    Path((guild, key)): Path<(String, String)>,
    form: FormBody,
) -> Response {
    let path = format!("/g/{guild}/apply/{key}");
    let session = match require_session(&h, &path) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let gid = match guild_access(&session, &guild) {
        Ok(g) => g,
        Err(r) => return *r,
    };
    if !crate::web::auth::csrf_ok(&session, form.0.get("csrf")) {
        return html(page("Expired", Some(&session), &note_err("That form expired. Reload the page and try again.")));
    }
    if !crate::web::auth::allow_action(&session.user.id, "apply", 5, 10 * 60 * 1000) {
        return html(page("Slow down", Some(&session), &note_err("That's a lot of applications in a short time. Try again in a few minutes.")));
    }
    // Re-read now, not from the rendered page: it may have closed in between.
    let Some(app) = find_app(&guild, &key) else {
        return html(page("Not found", Some(&session), &empty("That application doesn't exist.")));
    };
    if app.closed {
        return html(page(&app.label, Some(&session), &note_err("Applications closed before this went through.")));
    }
    if app.review_channel_id.is_empty() {
        return html(page(
            &app.label,
            Some(&session),
            &note_err("This application has no review channel set, so there's nowhere to send it. Tell a staff member."),
        ));
    }

    let answers = collect_answers(&form.0, app.questions.len());
    if answers.iter().all(|a| a.is_empty()) {
        return html(page(&app.label, Some(&session), &note_err("Every answer was blank, so nothing was sent.")));
    }

    let Some(ctx) = crate::DISCORD.get() else {
        return html(page(&app.label, Some(&session), &note_err("The bot isn't connected to Discord right now. Try again shortly.")));
    };
    let Ok(uid) = session.user.id.parse::<u64>() else {
        return html(page(&app.label, Some(&session), &note_err("Couldn't read your Discord id.")));
    };
    let Ok(user) = UserId::new(uid).to_user(&ctx.http).await else {
        return html(page(&app.label, Some(&session), &note_err("Couldn't load your Discord profile.")));
    };

    // started_at is now: a web form is filled in one pass, so the duration in
    // the review embed reflects submission rather than an interview length.
    let ok = finalize_application(ctx, gid, &user, &app, &answers, now_ms()).await;
    let body = if ok {
        format!(
            "<h1 style=\"margin:34px 0 12px;font-size:26px\">{}</h1>{}",
            esc(&app.label),
            note_ok("Sent. Staff will review it and you'll hear back in your Discord DMs.")
        )
    } else {
        format!(
            "<h1 style=\"margin:34px 0 12px;font-size:26px\">{}</h1>{}",
            esc(&app.label),
            note_err("I couldn't post it to the review channel. Tell a staff member the bot may be missing access there.")
        )
    };
    html(page(&app.label, Some(&session), &body))
}

// ── tickets ───────────────────────────────────────────────────

pub async fn ticket_form(h: HeaderMap, Path((guild, key)): Path<(String, String)>) -> Response {
    let path = format!("/g/{guild}/ticket/{key}");
    let session = match require_session(&h, &path) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if let Err(r) = guild_access(&session, &guild) {
        return *r;
    }
    let cfg = get_ticket_config(&guild);
    let Some(t) = cfg.types.iter().find(|t| t.key == key).cloned() else {
        return html(page("Not found", Some(&session), &empty("That ticket type doesn't exist.")));
    };
    let body = format!(
        r#"<h1 style="margin:34px 0 4px;font-size:26px">{emoji} {label}</h1>
           <p class="muted" style="margin:0 0 22px">This opens a private channel with the staff team.</p>
           <form method="post" class="card">{csrf}
             <div class="q"><label for="reason">Briefly describe your issue</label>
               <textarea id="reason" name="reason" rows="4" required></textarea></div>
             <button class="btn" type="submit">Open ticket</button>
           </form>"#,
        emoji = if t.emoji.is_empty() { "🎫".to_string() } else { esc(&t.emoji) },
        label = esc(&t.label),
        csrf = csrf_field(&session)
    );
    html(page(&t.label, Some(&session), &body))
}

pub async fn ticket_submit(
    h: HeaderMap,
    Path((guild, key)): Path<(String, String)>,
    form: FormBody,
) -> Response {
    let path = format!("/g/{guild}/ticket/{key}");
    let session = match require_session(&h, &path) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let gid = match guild_access(&session, &guild) {
        Ok(g) => g,
        Err(r) => return *r,
    };
    if !crate::web::auth::csrf_ok(&session, form.0.get("csrf")) {
        return html(page("Expired", Some(&session), &note_err("That form expired. Reload the page and try again.")));
    }
    if !crate::web::auth::allow_action(&session.user.id, "ticket", 3, 10 * 60 * 1000) {
        return html(page("Slow down", Some(&session), &note_err("You've opened several tickets just now. Give it a few minutes.")));
    }
    let reason = form.0.get("reason").map(|s| s.trim().to_string()).unwrap_or_default();
    if reason.is_empty() {
        return html(page("Open a ticket", Some(&session), &note_err("Please say a little about the issue first.")));
    }

    let Some(ctx) = crate::DISCORD.get() else {
        return html(page("Open a ticket", Some(&session), &note_err("The bot isn't connected to Discord right now. Try again shortly.")));
    };
    let Ok(uid) = session.user.id.parse::<u64>() else {
        return html(page("Open a ticket", Some(&session), &note_err("Couldn't read your Discord id.")));
    };
    let Ok(user) = UserId::new(uid).to_user(&ctx.http).await else {
        return html(page("Open a ticket", Some(&session), &note_err("Couldn't load your Discord profile.")));
    };

    let body = match open_ticket(ctx, gid, &user, &key, &reason).await {
        Ok(id) => note_ok(&format!("Your ticket is open in Discord: #{id}. Head over there to continue.")),
        Err(why) => note_err(&why),
    };
    html(page("Open a ticket", Some(&session), &body))
}

// ── staff review ──────────────────────────────────────────────

/// The review queue is a pointer back into Discord rather than a second copy
/// of the submissions: they live in the review channel as embeds, and that
/// stays the record. What this adds is one place to see what exists and act.
pub async fn review_list(h: HeaderMap, Path(guild): Path<String>) -> Response {
    let path = format!("/g/{guild}/review");
    let session = match require_session(&h, &path) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let gid = match guild_access(&session, &guild) {
        Ok(g) => g,
        Err(r) => return *r,
    };
    if !is_staff(&session, gid).await {
        return html(page("Review", Some(&session), &empty("That area is staff only.")));
    }

    let apps = get_applications(&guild);
    let mut pending = crate::state::applications::list_pending(&guild);
    // Newest first: the queue reads as a queue.
    pending.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));

    let mut cards = String::new();
    for p in &pending {
        let label = apps.get(&p.app_key).map(|a| a.label.clone()).unwrap_or_else(|| p.app_key.clone());
        let emoji = apps.get(&p.app_key).map(|a| a.emoji.clone()).unwrap_or_default();
        cards.push_str(&format!(
            r#"<div class="card">
  <div class="row"><h3>{emoji} {label}</h3><span class="pill pending">Awaiting review</span></div>
  <p style="margin:6px 0 2px">{who} <span class="muted">({uid})</span></p>
  <p class="muted" style="margin:0 0 14px">Submitted {when} ·
     <a style="text-decoration:underline" href="https://discord.com/channels/{g}/{ch}/{msg}">open in Discord</a></p>
  <form method="post" action="/g/{g}/review/{key}/{uid}">
    {csrf}
    <div class="q"><label for="r-{key}-{uid}">Reason (optional, shared with the applicant)</label>
      <input id="r-{key}-{uid}" type="text" name="reason" placeholder="Leave blank for no reason"></div>
    <div class="row">
      <button class="btn good" name="decision" value="accept" type="submit">Accept</button>
      <button class="btn bad" name="decision" value="deny" type="submit">Deny</button>
    </div>
  </form>
</div>"#,
            emoji = esc(&emoji),
            label = esc(&label),
            who = esc(&p.user_name),
            uid = esc(&p.user_id),
            when = esc(&crate::systems::tickets::format_utc(p.submitted_at / 1000)),
            g = esc(&guild),
            ch = esc(&p.channel_id),
            msg = esc(&p.message_id),
            key = esc(&p.app_key),
            csrf = csrf_field(&session),
        ));
    }

    let body = format!(
        r#"<h1 style="margin:34px 0 4px;font-size:26px">Review queue</h1>
           <p class="muted" style="margin:0 0 22px">{n} waiting. Deciding here does exactly what the buttons in
           Discord do: grants the roles, DMs the applicant, and marks the embed.</p>
           {cards}"#,
        n = pending.len(),
        cards = if cards.is_empty() {
            empty("Nothing waiting. Submissions made from Discord DMs show up here too.")
        } else {
            format!(r#"<div class="grid g2">{cards}</div>"#)
        }
    );
    html(page("Review queue", Some(&session), &body))
}

/// Accept or deny from the web. Re-checks reviewer rights at the moment of the
/// decision rather than trusting the page that offered the button.
pub async fn review_decide(
    h: HeaderMap,
    Path((guild, key, user)): Path<(String, String, String)>,
    form: FormBody,
) -> Response {
    let path = format!("/g/{guild}/review");
    let session = match require_session(&h, &path) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let gid = match guild_access(&session, &guild) {
        Ok(g) => g,
        Err(r) => return *r,
    };
    if !crate::web::auth::csrf_ok(&session, form.0.get("csrf")) {
        return html(page("Expired", Some(&session), &note_err("That form expired. Reload the page and try again.")));
    }
    let Some(app) = find_app(&guild, &key) else {
        return html(page("Review", Some(&session), &empty("That application doesn't exist.")));
    };
    if !may_review(&session, gid, &app).await {
        return html(page("Review", Some(&session), &empty("You're not allowed to review that one.")));
    }
    let accept = form.0.get("decision").map(|d| d == "accept").unwrap_or(false);
    let reason = form.0.get("reason").cloned().unwrap_or_default();

    let Some(ctx) = crate::DISCORD.get() else {
        return html(page("Review", Some(&session), &note_err("The bot isn't connected right now.")));
    };
    let Ok(target) = user.parse::<u64>() else {
        return html(page("Review", Some(&session), &note_err("That applicant id doesn't look right.")));
    };

    let outcome = crate::systems::applications::decide_from_web(
        ctx,
        gid,
        &app,
        UserId::new(target),
        accept,
        if reason.trim().is_empty() { None } else { Some(reason.trim().to_string()) },
        &session.user.id,
    )
    .await;

    let body = match outcome {
        Ok(msg) => note_ok(&msg),
        Err(why) => note_err(&why),
    };
    let back = format!(r#"{body}<a class="btn ghost" href="/g/{}/review">Back to the queue</a>"#, esc(&guild));
    html(page("Review", Some(&session), &back))
}
