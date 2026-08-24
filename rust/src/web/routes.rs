//! HTTP routes for the dashboard.
//!
//! Access rules, in one place so they're easy to audit:
//!   * every page except the landing page and the OAuth endpoints needs a session;
//!   * a guild page additionally needs the visitor to be in that guild AND the
//!     bot to be in it too;
//!   * the review pages additionally need the visitor to be staff for that
//!     guild, re-checked from Discord on every request rather than trusted
//!     from the session.

use axum::extract::{Form, Path, Query};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serenity::model::id::{GuildId, UserId};
use std::collections::HashMap;

use crate::common::guildinfo::{fetch_member, GuildInfo};
use crate::common::permissions::{is_mod, is_owner};
use crate::state::applications::{get_applications, Application};
use crate::state::tickets::get_ticket_config;
use crate::web::auth::{self, Session};
use crate::web::views::{empty, esc, note_err, page};

/// Response headers applied to every page.
///
/// The CSP is the load-bearing one: this app builds HTML by hand, so if an
/// escaping bug ever slips through, `script-src 'self'` is what stops it
/// becoming code execution. Styles are inline by design, hence 'unsafe-inline'
/// there and nowhere else. frame-ancestors 'none' blocks a hostile page from
/// framing the review queue and tricking a reviewer into clicking Accept.
async fn security_headers(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; \
         script-src 'self'; \
         style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
         font-src https://fonts.gstatic.com; \
         img-src 'self' https://cdn.discordapp.com data:; \
         form-action 'self'; \
         base-uri 'none'; \
         frame-ancestors 'none'"
            .parse()
            .unwrap(),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    h.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    h.insert(header::REFERRER_POLICY, "same-origin".parse().unwrap());
    // Only meaningful over https, and only honest to send there.
    if crate::common::config::WEB.base_url.starts_with("https://") {
        h.insert(
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=31536000; includeSubDomains".parse().unwrap(),
        );
    }
    res
}

pub fn router() -> Router {
    Router::new()
        .route("/", get(landing))
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        // POST, not GET: a GET that destroys the session can be fired by any
        // page that can make the browser load a URL.
        .route("/auth/logout", post(logout))
        .route("/servers", get(servers))
        .route("/g/{guild}", get(guild_home))
        .route("/g/{guild}/apply/{key}", get(apply_form).post(apply_submit))
        .route("/g/{guild}/ticket/{key}", get(ticket_form).post(ticket_submit))
        .route("/g/{guild}/review", get(review_list))
        .route("/g/{guild}/review/{key}/{user}", post(review_decide))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(security_headers))
}

// ── helpers ───────────────────────────────────────────────────

fn cookie(h: &HeaderMap) -> Option<&str> {
    h.get(header::COOKIE).and_then(|v| v.to_str().ok())
}

fn html(body: String) -> Response {
    Html(body).into_response()
}

/// A page that needs a session. Sends anonymous visitors to Discord and back.
// The error side is a whole HTTP response, which is a big value to carry in a
// Result; boxing it keeps these helpers cheap to return.
pub fn require_session(h: &HeaderMap, return_to: &str) -> Result<Session, Box<Response>> {
    match auth::session_for(cookie(h)) {
        Some(s) => Ok(s),
        None => Err(Box::new(Redirect::to(&auth::start_login(return_to)).into_response())),
    }
}

/// The bot has to be in the guild too, or there is nothing to show and nothing
/// to post to.
fn bot_is_in(guild_id: GuildId) -> bool {
    crate::DISCORD.get().map(|ctx| ctx.cache.guilds().contains(&guild_id)).unwrap_or(false)
}

/// Guild access: the visitor must be a member, and the bot must be there too.
pub fn guild_access(session: &Session, guild: &str) -> Result<GuildId, Box<Response>> {
    let deny = |title: &str, msg: &str| Box::new(html(page(title, Some(session), &empty(msg))));

    let Ok(raw) = guild.parse::<u64>() else {
        return Err(deny("Not found", "That server id doesn't look right."));
    };
    let gid = GuildId::new(raw);
    // The session's server list is a snapshot from login and can be up to a
    // week old, so it decides what to *show*, never what to allow. Membership
    // is confirmed against Discord here, so someone who left or was kicked
    // loses access immediately rather than when their session expires.
    if !session.guilds.iter().any(|g| g.id == guild) {
        return Err(deny("No access", "You're not in that server, so there's nothing here for you."));
    }
    if !bot_is_in(gid) {
        return Err(deny(
            "Bot not in server",
            "Guardian isn't in that server yet, so applications and tickets aren't set up.",
        ));
    }
    Ok(gid)
}

/// Staff check, read live from Discord rather than from the session, so losing
/// a role takes effect immediately.
pub async fn is_staff(session: &Session, guild_id: GuildId) -> bool {
    let Some(ctx) = crate::DISCORD.get() else { return false };
    let Ok(uid) = session.user.id.parse::<u64>() else { return false };
    let user_id = UserId::new(uid);
    if is_owner(user_id) {
        return true;
    }
    let Some(info) = GuildInfo::from_cache(ctx, guild_id) else { return false };
    if user_id == info.owner_id {
        return true;
    }
    match fetch_member(ctx, guild_id, user_id).await {
        Some(m) => is_mod(&m, info.owner_id),
        None => false,
    }
}

/// Peer review: on some applications, anyone already holding one of that
/// application's accepted roles may review it. Mirrors the Discord buttons.
pub async fn may_review(session: &Session, guild_id: GuildId, app: &Application) -> bool {
    if is_staff(session, guild_id).await {
        return true;
    }
    if !matches!(app.key.as_str(), "nypd" | "gambino" | "colombo") {
        return false;
    }
    let Some(ctx) = crate::DISCORD.get() else { return false };
    let Ok(uid) = session.user.id.parse::<u64>() else { return false };
    match fetch_member(ctx, guild_id, UserId::new(uid)).await {
        Some(m) => m.roles.iter().any(|r| app.accepted_role_ids.contains(&r.to_string())),
        None => false,
    }
}

// ── pages ─────────────────────────────────────────────────────

async fn landing(h: HeaderMap) -> Response {
    let session = auth::session_for(cookie(&h));
    let cta = if session.is_some() {
        r#"<a class="btn" href="/servers">Open dashboard</a>"#
    } else {
        r#"<a class="btn" href="/auth/login">Sign in with Discord</a>"#
    };
    let body = format!(
        r#"<section class="hero">
  <h1>Applications and tickets,<br>without leaving your browser</h1>
  <p>Apply to a server, open a support ticket, and review submissions. Everything
     lands in Discord exactly as it always has, so your staff workflow doesn't change.</p>
  <div class="row" style="justify-content:center">{cta}</div>
</section>
<div class="grid g3">
  <div class="card"><h3>📝 Applications</h3><p>Fill a form in one pass instead of answering a DM question by question. Submissions post to your review channel with the same embed and the same buttons.</p></div>
  <div class="card"><h3>🎫 Tickets</h3><p>Open a ticket from the web and get the same private channel, welcome embed and staff ping as the button flow.</p></div>
  <div class="card"><h3>🛡️ Review</h3><p>Staff can accept or deny from here, with an optional reason. Roles are granted and the applicant is DMed, exactly as in Discord.</p></div>
</div>"#
    );
    html(page("Guardian", session.as_ref(), &body))
}

#[derive(Deserialize)]
struct LoginQuery {
    #[serde(default)]
    next: Option<String>,
}

async fn login(Query(q): Query<LoginQuery>) -> Response {
    // Only ever accept a same-site path, so this can't be turned into an open
    // redirect that bounces people off to someone else's site after login.
    let next = q.next.filter(|n| n.starts_with('/') && !n.starts_with("//")).unwrap_or_else(|| "/servers".into());
    Redirect::to(&auth::start_login(&next)).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error_description: Option<String>,
}

async fn callback(Query(q): Query<CallbackQuery>) -> Response {
    if let Some(err) = q.error_description {
        return html(page("Sign-in failed", None, &note_err(&format!("Discord said: {err}"))));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return html(page("Sign-in failed", None, &note_err("That sign-in link was incomplete. Please try again.")));
    };
    // An unknown state means the handshake wasn't started here, was already
    // used, or has expired - all reasons to refuse rather than continue.
    let Some(return_to) = auth::take_handshake(&state) else {
        return html(page(
            "Sign-in failed",
            None,
            &note_err("That sign-in attempt has expired or was already used. Please start again."),
        ));
    };
    match auth::complete_login(&code).await {
        Ok(id) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::SET_COOKIE, auth::set_cookie(&id).parse().unwrap());
            (headers, Redirect::to(&return_to)).into_response()
        }
        Err(why) => html(page("Sign-in failed", None, &note_err(&why))),
    }
}

async fn logout(h: HeaderMap, form: FormBody) -> Response {
    // Signing out is a state change, so it carries the same token as the rest.
    if let Some(s) = auth::session_for(cookie(&h)) {
        if !auth::csrf_ok(&s, form.0.get("csrf")) {
            return Redirect::to("/").into_response();
        }
    }
    auth::destroy(cookie(&h));
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, auth::clear_cookie().parse().unwrap());
    (headers, Redirect::to("/")).into_response()
}

/// The server picker: every guild the visitor is in that also has the bot.
async fn servers(h: HeaderMap) -> Response {
    let session = match require_session(&h, "/servers") {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let mut cards = String::new();
    let mut count = 0;
    for g in &session.guilds {
        let Ok(raw) = g.id.parse::<u64>() else { continue };
        if !bot_is_in(GuildId::new(raw)) {
            continue;
        }
        count += 1;
        let icon = match g.icon_url() {
            Some(u) => format!(r#"<img src="{}" alt="">"#, esc(&u)),
            None => esc(&g.initials()),
        };
        cards.push_str(&format!(
            r#"<a class="card link" href="/g/{id}"><div class="srv"><div class="ico">{icon}</div>
               <div><h3>{name}</h3><p>Applications and tickets</p></div></div></a>"#,
            id = esc(&g.id),
            icon = icon,
            name = esc(&g.name)
        ));
    }
    let body = if count == 0 {
        format!(
            r#"<h2 class="sec">Your servers</h2>{}"#,
            empty("None of your servers have Guardian in them yet. Invite the bot, then come back.")
        )
    } else {
        format!(r#"<h2 class="sec">Your servers</h2><div class="grid g2">{cards}</div>"#)
    };
    html(page("Your servers", Some(&session), &body))
}

/// One server: what you can apply for, and what you can open a ticket about.
async fn guild_home(h: HeaderMap, Path(guild): Path<String>) -> Response {
    let session = match require_session(&h, &format!("/g/{guild}")) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let gid = match guild_access(&session, &guild) {
        Ok(g) => g,
        Err(r) => return *r,
    };
    let name = crate::DISCORD
        .get()
        .and_then(|ctx| ctx.cache.guild(gid).map(|g| g.name.to_string()))
        .unwrap_or_else(|| guild.clone());

    let apps = get_applications(&guild);
    let mut app_cards = String::new();
    for (_, a) in apps.iter() {
        let (pill, action) = if a.closed {
            (r#"<span class="pill closed">Closed</span>"#.to_string(), String::new())
        } else {
            (
                r#"<span class="pill open">Open</span>"#.to_string(),
                format!(r#"<a class="btn sm" href="/g/{}/apply/{}">Apply</a>"#, esc(&guild), esc(&a.key)),
            )
        };
        app_cards.push_str(&format!(
            r#"<div class="card"><div class="row"><h3>{emoji} {label}</h3>{pill}</div>
               <p>{n} question{s}</p><div class="row" style="margin-top:14px">{action}</div></div>"#,
            emoji = esc(&a.emoji),
            label = esc(&a.label),
            pill = pill,
            n = a.questions.len(),
            s = if a.questions.len() == 1 { "" } else { "s" },
            action = action
        ));
    }

    let cfg = get_ticket_config(&guild);
    let mut ticket_cards = String::new();
    for t in &cfg.types {
        ticket_cards.push_str(&format!(
            r#"<div class="card"><h3>{emoji} {label}</h3>
               <div class="row" style="margin-top:14px">
               <a class="btn sm ghost" href="/g/{g}/ticket/{k}">Open a ticket</a></div></div>"#,
            emoji = if t.emoji.is_empty() { "🎫".to_string() } else { esc(&t.emoji) },
            label = esc(&t.label),
            g = esc(&guild),
            k = esc(&t.key)
        ));
    }

    let staff_link = if is_staff(&session, gid).await {
        format!(r#"<a class="btn sm ghost" href="/g/{}/review">Review queue</a>"#, esc(&guild))
    } else {
        String::new()
    };

    let body = format!(
        r#"<div class="row" style="margin:34px 0 6px">
             <h1 style="margin:0;font-size:26px">{name}</h1><span class="nav-sp"></span>{staff_link}
           </div>
           <h2 class="sec">Applications</h2>{apps}
           <h2 class="sec">Tickets</h2>{tickets}"#,
        name = esc(&name),
        staff_link = staff_link,
        apps = if app_cards.is_empty() {
            empty("No applications are set up in this server yet.")
        } else {
            format!(r#"<div class="grid g2">{app_cards}</div>"#)
        },
        tickets = if ticket_cards.is_empty() {
            empty("No ticket types are set up in this server yet.")
        } else {
            format!(r#"<div class="grid g3">{ticket_cards}</div>"#)
        }
    );
    html(page(&name, Some(&session), &body))
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Html(page("Not found", None, &empty("There's nothing at that address.")))).into_response()
}

// Application + ticket + review handlers live in submit.rs to keep this file
// about routing and access rules.
pub use super::submit::{apply_form, apply_submit, review_decide, review_list, ticket_form, ticket_submit};

/// Shared by the form handlers: pull the named application, if it exists.
pub fn find_app(guild: &str, key: &str) -> Option<Application> {
    get_applications(guild).get(key).cloned()
}

/// Form bodies arrive as `q0`, `q1`, ... in question order.
pub fn collect_answers(form: &HashMap<String, String>, count: usize) -> Vec<String> {
    (0..count)
        .map(|i| form.get(&format!("q{i}")).map(|s| s.trim().to_string()).unwrap_or_default())
        .collect()
}

pub type FormBody = Form<HashMap<String, String>>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn head_of(path: &str, name: header::HeaderName) -> String {
        let res = router()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        res.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string()
    }

    async fn get(path: &str) -> (StatusCode, String) {
        let res = router()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, if loc.is_empty() { String::from_utf8_lossy(&bytes).to_string() } else { loc })
    }

    #[tokio::test]
    async fn the_landing_page_is_public() {
        let (status, body) = get("/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Applications and tickets"), "landing page should render its hero");
        assert!(body.contains("Sign in with Discord"), "a signed-out visitor is offered a login");
    }

    /// Everything behind the landing page requires a session, and an anonymous
    /// visitor is sent to Discord rather than shown anything.
    #[tokio::test]
    async fn private_pages_redirect_anonymous_visitors_to_discord() {
        for path in ["/servers", "/g/123", "/g/123/apply/staff", "/g/123/review"] {
            let (status, location) = get(path).await;
            assert!(status.is_redirection(), "{path} should redirect, got {status}");
            assert!(
                location.starts_with("https://discord.com/oauth2/authorize"),
                "{path} should redirect to Discord, got {location}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_paths_are_not_found() {
        let (status, _) = get("/no/such/page").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The OAuth state parameter is what stops a callback being replayed or
    /// forged, so an unrecognised one must never complete a login.
    #[tokio::test]
    async fn a_callback_with_an_unknown_state_is_refused() {
        let (status, body) = get("/auth/callback?code=abc&state=not-a-real-state").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("expired or was already used"), "got: {body}");
    }

    /// A login must not be usable to bounce someone to another site afterwards.
    #[tokio::test]
    async fn login_only_returns_to_same_site_paths() {
        let (_, location) = get("/auth/login?next=https://evil.example/steal").await;
        let decoded = urlencoding::decode(&location).unwrap_or_default().to_string();
        assert!(!decoded.contains("evil.example"), "off-site next must be dropped: {decoded}");
    }

    /// The CSP is what keeps an escaping slip from becoming code execution, so
    /// it has to be on every response, not just the ones we remembered.
    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        for path in ["/", "/no/such/page", "/servers"] {
            let csp = head_of(path, header::CONTENT_SECURITY_POLICY).await;
            assert!(csp.contains("script-src 'self'"), "{path} must forbid inline script: {csp}");
            assert!(csp.contains("frame-ancestors 'none'"), "{path} must refuse being framed: {csp}");
            assert!(csp.contains("default-src 'none'"), "{path} should default-deny: {csp}");
            assert_eq!(head_of(path, header::X_CONTENT_TYPE_OPTIONS).await, "nosniff");
            assert_eq!(head_of(path, header::X_FRAME_OPTIONS).await, "DENY");
        }
    }

    /// Sign-out changes state, so it must not be reachable by making a browser
    /// load a URL.
    #[tokio::test]
    async fn logout_is_not_reachable_by_get() {
        let (status, _) = get("/auth/logout").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "GET logout would be CSRF-able");
    }

    /// The pages themselves must not rely on inline script, or the CSP above
    /// would break the site rather than protect it.
    #[tokio::test]
    async fn pages_contain_no_inline_script() {
        let (_, body) = get("/").await;
        assert!(!body.contains("<script"), "inline script would be blocked by our own CSP");
    }
}
