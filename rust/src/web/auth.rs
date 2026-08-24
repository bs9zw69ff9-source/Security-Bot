//! Discord OAuth2 login and server-side sessions.
//!
//! Sessions live in memory only. A restart logs everyone out, which is the
//! right trade for a dashboard: nothing here is worth persisting, and it means
//! an access token never touches disk.

use once_cell::sync::Lazy;
use rand::RngExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::common::config::{now_ms, WEB};

/// How long a login lasts, and how long an in-flight OAuth handshake may take.
const SESSION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const HANDSHAKE_TTL_MS: i64 = 10 * 60 * 1000;
pub const COOKIE: &str = "guardian_session";

#[derive(Clone, Deserialize)]
pub struct DiscordUser {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
}

impl DiscordUser {
    pub fn display_name(&self) -> String {
        self.global_name.clone().unwrap_or_else(|| self.username.clone())
    }
    pub fn avatar_url(&self) -> String {
        match &self.avatar {
            Some(hash) => format!("https://cdn.discordapp.com/avatars/{}/{hash}.png?size=64", self.id),
            // Discord's default avatars are indexed off the id for migrated
            // accounts; index 0 is a safe stand-in for everyone else.
            None => "https://cdn.discordapp.com/embed/avatars/0.png".to_string(),
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct UserGuild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

impl UserGuild {
    pub fn icon_url(&self) -> Option<String> {
        self.icon.as_ref().map(|h| format!("https://cdn.discordapp.com/icons/{}/{h}.png?size=64", self.id))
    }
    /// First letters of the name, for servers with no icon.
    pub fn initials(&self) -> String {
        self.name.split_whitespace().filter_map(|w| w.chars().next()).take(2).collect::<String>().to_uppercase()
    }
}

#[derive(Clone)]
pub struct Session {
    pub user: DiscordUser,
    pub guilds: Vec<UserGuild>,
    pub expires_at: i64,
    /// Per-session CSRF token, embedded in every form that changes something.
    ///
    /// SameSite=Lax already stops a cross-site POST carrying the cookie, so
    /// this is the second lock rather than the only one: it still holds if the
    /// cookie policy is ever loosened, and it covers browsers that treat Lax
    /// inconsistently.
    pub csrf: String,
}

static SESSIONS: Lazy<Mutex<HashMap<String, Session>>> = Lazy::new(|| Mutex::new(HashMap::new()));
/// state -> (expiry, where to send them afterwards)
static HANDSHAKES: Lazy<Mutex<HashMap<String, (i64, String)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn sessions() -> std::sync::MutexGuard<'static, HashMap<String, Session>> {
    match SESSIONS.lock() { Ok(g) => g, Err(e) => e.into_inner() }
}
fn handshakes() -> std::sync::MutexGuard<'static, HashMap<String, (i64, String)>> {
    match HANDSHAKES.lock() { Ok(g) => g, Err(e) => e.into_inner() }
}

/// 32 bytes of randomness, hex encoded. Used for both session ids and the
/// OAuth state parameter, so neither is guessable.
fn token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Begin a login. Returns the Discord URL to send the browser to.
pub fn start_login(return_to: &str) -> String {
    let state = token();
    {
        let mut h = handshakes();
        h.retain(|_, (exp, _)| *exp > now_ms());
        h.insert(state.clone(), (now_ms() + HANDSHAKE_TTL_MS, return_to.to_string()));
    }
    format!(
        "https://discord.com/oauth2/authorize?client_id={}&redirect_uri={}&response_type=code&scope=identify%20guilds&state={}&prompt=none",
        urlencoding::encode(&WEB.client_id),
        urlencoding::encode(&WEB.redirect_uri()),
        urlencoding::encode(&state),
    )
}

/// Consume a state value. `None` means it was never issued, already used, or
/// expired, all of which are grounds to refuse the callback.
pub fn take_handshake(state: &str) -> Option<String> {
    let mut h = handshakes();
    h.retain(|_, (exp, _)| *exp > now_ms());
    h.remove(state).map(|(_, return_to)| return_to)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Swap the authorization code for a token, then read who it belongs to.
/// The token is used here and dropped; only the resulting profile is kept.
pub async fn complete_login(code: &str) -> Result<String, String> {
    let http = reqwest::Client::new();
    let redirect = WEB.redirect_uri();
    let form = [
        ("client_id", WEB.client_id.as_str()),
        ("client_secret", WEB.client_secret.as_str()),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect.as_str()),
    ];
    let res = http
        .post("https://discord.com/api/v10/oauth2/token")
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("couldn't reach Discord to exchange the login code: {e}"))?;
    if !res.status().is_success() {
        // Logged in full, shown to the user only as a status: the body can
        // echo back request details and is not the visitor's business.
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        eprintln!("⚠️ OAuth token exchange failed ({status}): {body}");
        return Err(format!("Discord rejected that sign-in ({status}). Please try again."));
    }
    let token_res: TokenResponse =
        res.json().await.map_err(|e| format!("couldn't read Discord's token response: {e}"))?;

    let user: DiscordUser = http
        .get("https://discord.com/api/v10/users/@me")
        .bearer_auth(&token_res.access_token)
        .send()
        .await
        .map_err(|e| format!("couldn't load your Discord profile: {e}"))?
        .json()
        .await
        .map_err(|e| format!("couldn't read your Discord profile: {e}"))?;

    // A failure here is not fatal: it only means the server list is empty and
    // the dashboard says so, rather than the whole login failing.
    let guilds: Vec<UserGuild> = match http
        .get("https://discord.com/api/v10/users/@me/guilds")
        .bearer_auth(&token_res.access_token)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(e) => {
            eprintln!("⚠️ couldn't list {}'s servers: {e}", user.username);
            Vec::new()
        }
    };

    let id = token();
    let mut s = sessions();
    s.retain(|_, sess| sess.expires_at > now_ms());
    s.insert(
        id.clone(),
        Session { user, guilds, expires_at: now_ms() + SESSION_TTL_MS, csrf: token() },
    );
    Ok(id)
}

pub fn session_for(cookie_header: Option<&str>) -> Option<Session> {
    let raw = cookie_header?;
    let id = raw
        .split(';')
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())?;
    let mut s = sessions();
    s.retain(|_, sess| sess.expires_at > now_ms());
    s.get(&id).cloned()
}

pub fn destroy(cookie_header: Option<&str>) {
    let Some(raw) = cookie_header else { return };
    if let Some((_, id)) = raw.split(';').filter_map(|p| p.trim().split_once('=')).find(|(k, _)| *k == COOKIE) {
        sessions().remove(id);
    }
}

/// Secure is set whenever the site is served over https, which it always is in
/// a real deployment. Left off for a plain-http localhost so testing works.
pub fn set_cookie(id: &str) -> String {
    let secure = if WEB.base_url.starts_with("https://") { "; Secure" } else { "" };
    format!("{COOKIE}={id}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}", SESSION_TTL_MS / 1000)
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Constant-time-ish comparison for the CSRF token. Length is not secret and
/// the token is single-purpose, but there is no reason to leak position.
pub fn csrf_ok(session: &Session, supplied: Option<&String>) -> bool {
    let Some(given) = supplied else { return false };
    if given.len() != session.csrf.len() {
        return false;
    }
    given.bytes().zip(session.csrf.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

// ── abuse limits ──────────────────────────────────────────────
//
// Submitting posts an embed to a review channel and opening a ticket creates a
// Discord channel, so an unthrottled endpoint is a way to flood a server using
// the bot's own permissions. Keyed per user per action.

static ACTIONS: Lazy<Mutex<HashMap<String, Vec<i64>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Returns false when the caller has run out of allowance for `what`.
pub fn allow_action(user_id: &str, what: &str, limit: usize, window_ms: i64) -> bool {
    let now = now_ms();
    let mut map = match ACTIONS.lock() { Ok(g) => g, Err(e) => e.into_inner() };
    let key = format!("{user_id}:{what}");
    let hits = map.entry(key).or_default();
    hits.retain(|t| now - *t < window_ms);
    if hits.len() >= limit {
        return false;
    }
    hits.push(now);
    true
}
