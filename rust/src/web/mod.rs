//! The web dashboard.
//!
//! Runs inside the bot process on purpose. Guild settings, applications and
//! tickets are held in memory here and only mirrored to SQLite, so a separate
//! web service would keep its own copy of all of it and the two would
//! overwrite each other's writes.

pub mod auth;
pub mod views;
pub mod routes;
pub mod submit;

use crate::common::config::WEB;

/// Start the dashboard, if it's switched on and configured.
///
/// Never fatal: a misconfigured dashboard leaves the bot running normally and
/// says what is missing, rather than taking Discord down with it.
pub async fn serve() {
    if !WEB.enabled {
        return;
    }
    let missing = WEB.missing();
    if !missing.is_empty() {
        eprintln!("⚠️ WEB_ENABLED is on but {} not set, so the dashboard is off.", missing.join(" and "));
        return;
    }

    let app = routes::router();
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], WEB.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("⚠️ dashboard couldn't bind {addr}: {e}");
            return;
        }
    };
    println!("🌐 Dashboard on http://{addr} (public URL {})", WEB.base_url);
    println!("   OAuth redirect must be registered as: {}", WEB.redirect_uri());
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("⚠️ dashboard stopped: {e}");
    }
}
