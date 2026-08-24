//! HTML rendering.
//!
//! Hand-written rather than templated: there are a dozen pages, and a template
//! engine would be another dependency and another thing to keep in step with
//! the types. Everything user-supplied goes through `esc`.

use crate::web::auth::Session;

/// Escape text for HTML. Applied to every value that came from a user:
/// server names, usernames, question text, answers, ticket reasons.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

pub const CSS: &str = r#"
:root{
  --bg:#0b0d12; --panel:#141821; --panel-2:#1b202b; --line:#252b38;
  --text:#e8ecf5; --muted:#98a2b8; --accent:#5865f2; --accent-2:#4451e0;
  --good:#57f287; --bad:#ed4245; --warn:#f59e0b; --radius:14px;
}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--text);
  font:15px/1.6 "Inter",system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;
  -webkit-font-smoothing:antialiased}
a{color:inherit;text-decoration:none}
.wrap{max-width:1080px;margin:0 auto;padding:0 20px}
header.nav{border-bottom:1px solid var(--line);background:rgba(11,13,18,.9);
  backdrop-filter:blur(8px);position:sticky;top:0;z-index:10}
.nav-in{display:flex;align-items:center;gap:16px;height:64px}
.brand{display:flex;align-items:center;gap:10px;font-weight:700;letter-spacing:-.01em}
.brand .dot{width:26px;height:26px;border-radius:8px;
  background:linear-gradient(135deg,var(--accent),#8b5cf6);display:grid;place-items:center;font-size:14px}
.nav-sp{flex:1}
.me{display:flex;align-items:center;gap:9px;color:var(--muted);font-size:14px}
.me img{width:28px;height:28px;border-radius:50%}
.btn{display:inline-flex;align-items:center;gap:8px;background:var(--accent);color:#fff;
  border:0;border-radius:10px;padding:10px 16px;font:inherit;font-weight:600;cursor:pointer;
  transition:background .15s}
.btn:hover{background:var(--accent-2)}
.btn.ghost{background:transparent;border:1px solid var(--line);color:var(--text)}
.btn.ghost:hover{background:var(--panel-2)}
.btn.good{background:var(--good);color:#06210f}
.btn.bad{background:var(--bad)}
.btn.sm{padding:7px 12px;font-size:14px}
.hero{padding:72px 0 44px;text-align:center}
.hero h1{font-size:44px;line-height:1.15;margin:0 0 14px;letter-spacing:-.02em}
.hero p{color:var(--muted);font-size:18px;max-width:620px;margin:0 auto 26px}
.grid{display:grid;gap:16px}
.g2{grid-template-columns:repeat(auto-fit,minmax(280px,1fr))}
.g3{grid-template-columns:repeat(auto-fit,minmax(220px,1fr))}
.card{background:var(--panel);border:1px solid var(--line);border-radius:var(--radius);padding:20px}
.card h3{margin:0 0 6px;font-size:17px}
.card p{margin:0;color:var(--muted);font-size:14px}
.card.link:hover{border-color:var(--accent);background:var(--panel-2)}
.srv{display:flex;align-items:center;gap:13px}
.srv .ico{width:44px;height:44px;border-radius:12px;background:var(--panel-2);
  display:grid;place-items:center;font-weight:700;color:var(--muted);overflow:hidden;flex:none}
.srv .ico img{width:100%;height:100%;object-fit:cover}
h2.sec{font-size:13px;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);
  margin:34px 0 14px;font-weight:600}
.pill{display:inline-block;padding:3px 9px;border-radius:999px;font-size:12px;font-weight:600}
.pill.open{background:rgba(87,242,135,.14);color:var(--good)}
.pill.closed{background:rgba(237,66,69,.14);color:var(--bad)}
.pill.pending{background:rgba(245,158,11,.14);color:var(--warn)}
label{display:block;margin:0 0 7px;font-weight:600;font-size:14px}
.q{margin:0 0 22px}
.q .n{color:var(--accent);font-weight:700;margin-right:6px}
textarea,input[type=text]{width:100%;background:var(--panel-2);border:1px solid var(--line);
  color:var(--text);border-radius:10px;padding:11px 13px;font:inherit;resize:vertical}
textarea:focus,input:focus{outline:0;border-color:var(--accent)}
.req{color:var(--muted);font-size:13px;margin:0 0 20px;padding:12px 14px;
  background:var(--panel-2);border-radius:10px;border-left:3px solid var(--accent)}
.note{padding:13px 15px;border-radius:10px;margin:0 0 18px;font-size:14px}
.note.err{background:rgba(237,66,69,.12);border:1px solid rgba(237,66,69,.3);color:#ffb4b6}
.note.ok{background:rgba(87,242,135,.1);border:1px solid rgba(87,242,135,.28);color:#a9f7c4}
.ans{background:var(--panel-2);border-radius:8px;padding:10px 12px;margin:5px 0 0;
  white-space:pre-wrap;font-size:14px}
.muted{color:var(--muted)}
footer{border-top:1px solid var(--line);margin-top:56px;padding:22px 0;color:var(--muted);font-size:13px}
.row{display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.empty{text-align:center;padding:52px 20px;color:var(--muted)}
@media(max-width:640px){.hero h1{font-size:32px}.nav-in{height:56px}}
"#;

/// Page shell. `session` drives the nav, so every page shows who is signed in.
pub fn page(title: &str, session: Option<&Session>, body: &str) -> String {
    let right = match session {
        Some(s) => format!(
            r#"<span class="me"><img src="{}" alt=""> {}</span>
               <a class="btn ghost sm" href="/auth/logout">Sign out</a>"#,
            esc(&s.user.avatar_url()),
            esc(&s.user.display_name())
        ),
        None => r#"<a class="btn sm" href="/auth/login">Sign in with Discord</a>"#.to_string(),
    };
    format!(
        r#"<!doctype html><html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{t} · Guardian</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;600;700&display=swap" rel="stylesheet">
<style>{css}</style></head><body>
<header class="nav"><div class="wrap nav-in">
  <a class="brand" href="/"><span class="dot">🛡</span> Guardian</a>
  <span class="nav-sp"></span>{right}
</div></header>
<main class="wrap">{body}</main>
<footer class="wrap">Guardian · applications and tickets for Discord</footer>
</body></html>"#,
        t = esc(title),
        css = CSS,
        right = right,
        body = body
    )
}

pub fn note_err(msg: &str) -> String {
    format!(r#"<p class="note err">{}</p>"#, esc(msg))
}
pub fn note_ok(msg: &str) -> String {
    format!(r#"<p class="note ok">{}</p>"#, esc(msg))
}
pub fn empty(msg: &str) -> String {
    format!(r#"<div class="empty">{}</div>"#, esc(msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every page is assembled by hand, so escaping is the only thing standing
    /// between a server name or an application answer and script injection.
    #[test]
    fn user_content_cannot_break_out_of_the_page() {
        let nasty = r#"<script>alert('x')</script>" onmouseover="evil()"#;
        let out = esc(nasty);
        assert!(!out.contains('<'), "angle brackets must be escaped: {out}");
        assert!(!out.contains('>'), "angle brackets must be escaped: {out}");
        assert!(!out.contains('"'), "quotes must be escaped so attributes can't be broken: {out}");
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn ampersands_are_escaped_once_and_only_once() {
        assert_eq!(esc("Tom & Jerry"), "Tom &amp; Jerry");
        assert_eq!(esc("&amp;"), "&amp;amp;");
    }

    #[test]
    fn a_signed_out_page_offers_a_login_and_no_profile() {
        let html = page("Test", None, "<p>hi</p>");
        assert!(html.contains("Sign in with Discord"));
        assert!(!html.contains("Sign out"));
        assert!(html.contains("<p>hi</p>"));
    }
}
