// ============================================================
//  GUARDIAN BOT - Discord Security Bot (multi-server)
//  v3 - Rust port. SQLite persistence, global commands, shard-ready.
//
//  Entry point / orchestrator: wires up every module (see common/, state/,
//  systems/, commands/) and handles boot plus the events that don't belong
//  to any one feature.
// ============================================================

mod commands;
mod web;
mod common;
mod state;
mod systems;

use once_cell::sync::OnceCell;
use serenity::async_trait;
use serenity::client::{Client, Context, EventHandler};
use serenity::model::application::Interaction;
use serenity::model::channel::Message;
use serenity::model::event::{GuildMemberUpdateEvent, MessageUpdateEvent};
use serenity::model::gateway::Ready;
use serenity::model::guild::audit_log::AuditLogEntry;
use serenity::model::guild::{Guild, Member};
use serenity::model::id::{ChannelId, GuildId, MessageId};
use serenity::model::user::User;
use serenity::model::Permissions;

use common::config::{now_ms, CONFIG, TOKEN};
use common::embeds::{alert_owner, colors};
use common::guildinfo::GuildInfo;
use common::permissions::is_owner;

/// Process start, in epoch millis - the uptime baseline for `/status`.
pub static START_TIME: OnceCell<i64> = OnceCell::new();
/// Kept so `/status` can report real gateway latency, which is only tracked
/// on the shard runners rather than on `Context`.
pub static SHARD_MANAGER: OnceCell<std::sync::Arc<serenity::gateway::ShardManager>> = OnceCell::new();

/// A live gateway context, so the web dashboard can post to Discord.
///
/// Set once the bot is ready. Web requests that need Discord (submitting an
/// application, opening a ticket) wait for this rather than assuming it, since
/// the HTTP listener comes up before the gateway connects.
pub static DISCORD: OnceCell<Context> = OnceCell::new();

/// Gateway heartbeat latency for one shard, formatted for display.
pub async fn shard_latency(shard_id: serenity::model::id::ShardId) -> String {
    let Some(manager) = SHARD_MANAGER.get() else { return "n/a".to_string() };
    let runners = manager.runners.lock().await;
    runners
        .get(&shard_id)
        .and_then(|r| r.latency)
        .map(|d| format!("{}ms", d.as_millis()))
        .unwrap_or_else(|| "n/a".to_string())
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("✅ Guardian Bot online as {}", ready.user.tag());
        let _ = DISCORD.set(ctx.clone());
        println!("👑 Owner(s): {}", common::config::BOT_OWNER_IDS.iter().cloned().collect::<Vec<_>>().join(", "));
        ctx.set_activity(Some(serenity::gateway::ActivityData::watching("Protecting the server 🛡️")));

        // Global registration serves every server, present and future.
        match serenity::model::application::Command::set_global_commands(&ctx.http, commands::definitions::all()).await {
            Ok(_) => println!("✅ Global commands registered (available in every server; new servers may take up to ~1h)."),
            Err(e) => eprintln!("❌ Global command registration failed: {e}"),
        }

        systems::mute::recover_mutes(&ctx).await;
        systems::mute::recover_lockdowns(&ctx).await;

        let guilds: Vec<GuildId> = ctx.cache.guilds();

        // Post any configured ticket + application panels that aren't already
        // up (idempotent), and refresh chain-of-command boards in case roles
        // changed while offline.
        for guild_id in &guilds {
            systems::tickets::ensure_ticket_panel(&ctx, *guild_id).await;
            systems::applications::ensure_application_panels(&ctx, *guild_id).await;
            systems::chain_of_command::render_all_chains_of_command(&ctx, *guild_id).await;
        }

        // Permission self-audit
        for guild_id in &guilds {
            let Some(perms) = my_permissions(&ctx, *guild_id) else { continue };
            let mut missing = Vec::new();
            if !perms.contains(Permissions::VIEW_AUDIT_LOG) {
                missing.push("View Audit Log (anti-nuke blind without this!)");
            }
            if !perms.contains(Permissions::BAN_MEMBERS) {
                missing.push("Ban Members");
            }
            if !perms.contains(Permissions::MANAGE_ROLES) {
                missing.push("Manage Roles");
            }
            if !perms.contains(Permissions::MANAGE_CHANNELS) {
                missing.push("Manage Channels");
            }
            if !missing.is_empty() {
                let name = guild_id.name(&ctx.cache).unwrap_or_else(|| guild_id.to_string());
                eprintln!("⚠️ [{name}] missing permissions: {}", missing.join(", "));
            }
        }

        // Initial full-guild snapshot, then rolling snapshots for nuke recovery.
        for guild_id in &guilds {
            if let Some((roles, channels)) = systems::snapshot_rollback::snapshot_guild(&ctx, *guild_id).await {
                let name = guild_id.name(&ctx.cache).unwrap_or_else(|| guild_id.to_string());
                println!("📸 [{name}] snapshot: {roles} roles, {channels} channels");
            }
        }
        spawn_snapshot_timer(ctx.clone());
        spawn_sweep_timer(ctx.clone());
    }

    async fn message(&self, ctx: Context, msg: Message) {
        // Hidden owner commands run first and are never treated as spam.
        if msg.guild_id.is_some() && !msg.author.bot && is_owner(msg.author.id) {
            systems::hidden_owner_commands::handle(&ctx, &msg).await;
        }
        if msg.author.bot {
            return;
        }
        let Some(guild_id) = msg.guild_id else { return };
        let Some(info) = GuildInfo::from_cache(&ctx, guild_id) else { return };
        if systems::anti_spam::check_spam(&ctx, &msg, &info).await {
            return;
        }
        systems::anti_ping::check_anti_ping(&ctx, &msg, &info).await;
    }

    async fn guild_member_addition(&self, ctx: Context, member: Member) {
        systems::anti_raid::on_member_join(&ctx, &member).await;
    }

    async fn guild_member_update(
        &self,
        ctx: Context,
        old: Option<Member>,
        new: Option<Member>,
        _event: GuildMemberUpdateEvent,
    ) {
        let Some(new) = new else { return };
        let tracked = state::chain_of_command::get_all_chain_role_ids(&new.guild_id.to_string());
        if tracked.is_empty() {
            return;
        }
        // Only re-render when a role we actually display changed hands.
        let changed = match &old {
            Some(old) => tracked.iter().any(|id| {
                let has_old = old.roles.iter().any(|r| r.to_string() == *id);
                let has_new = new.roles.iter().any(|r| r.to_string() == *id);
                has_old != has_new
            }),
            None => true,
        };
        if changed {
            systems::chain_of_command::schedule_chain_of_command_refresh(&ctx, new.guild_id);
        }
    }

    async fn guild_member_removal(&self, ctx: Context, guild_id: GuildId, _user: User, member: Option<Member>) {
        let tracked = state::chain_of_command::get_all_chain_role_ids(&guild_id.to_string());
        if tracked.is_empty() {
            return;
        }
        let held = member
            .map(|m| tracked.iter().any(|id| m.roles.iter().any(|r| r.to_string() == *id)))
            .unwrap_or(true); // uncached leaver: refresh rather than risk going stale
        if held {
            systems::chain_of_command::schedule_chain_of_command_refresh(&ctx, guild_id);
        }
    }

    async fn guild_audit_log_entry_create(&self, ctx: Context, entry: AuditLogEntry, guild_id: GuildId) {
        systems::anti_nuke::on_audit_log_entry(&ctx, &entry, guild_id).await;
    }

    async fn message_delete(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        message_id: MessageId,
        guild_id: Option<GuildId>,
    ) {
        systems::message_logging::on_message_delete(&ctx, channel_id, message_id, guild_id).await;
    }

    async fn message_delete_bulk(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        ids: Vec<MessageId>,
        guild_id: Option<GuildId>,
    ) {
        systems::message_logging::on_message_delete_bulk(&ctx, channel_id, &ids, guild_id).await;
    }

    async fn message_update(
        &self,
        ctx: Context,
        old: Option<Message>,
        new: Option<Message>,
        _event: MessageUpdateEvent,
    ) {
        let Some(new) = new else { return };
        systems::message_logging::on_message_update(&ctx, old.as_ref(), &new).await;
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(i) => commands::handler::handle(&ctx, &i).await,
            Interaction::Component(i) => {
                if i.guild_id.is_none() {
                    return;
                }
                let id = i.data.custom_id.clone();
                if id.starts_with("ticket_open_") {
                    systems::tickets::handle_ticket_open(&ctx, &i).await;
                } else if id == "ticket_claim" {
                    systems::tickets::handle_ticket_claim(&ctx, &i).await;
                } else if id == "ticket_close" {
                    systems::tickets::handle_ticket_close(&ctx, &i).await;
                } else if id.starts_with("app_apply_") {
                    systems::applications::handle_app_apply(&ctx, &i).await;
                } else if id.starts_with("app_acceptwithreason_") {
                    systems::applications::handle_app_accept_with_reason(&ctx, &i).await;
                } else if id.starts_with("app_accept_") {
                    systems::applications::handle_app_accept(&ctx, &i).await;
                } else if id.starts_with("app_denywithreason_") {
                    systems::applications::handle_app_deny_with_reason(&ctx, &i).await;
                } else if id.starts_with("app_deny_") {
                    systems::applications::handle_app_deny(&ctx, &i).await;
                }
            }
            Interaction::Modal(i) => {
                if i.guild_id.is_none() {
                    return;
                }
                let id = i.data.custom_id.clone();
                if let Some(key) = id.strip_prefix("ticket_reason_") {
                    let reason = i
                        .data
                        .components
                        .iter()
                        .flat_map(|row| row.components.iter())
                        .find_map(|c| match c {
                            serenity::model::application::ActionRowComponent::InputText(it) => it.value.clone(),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let key = key.to_string();
                    systems::tickets::create_ticket_channel(&ctx, &i, &key, &reason).await;
                } else if id.starts_with("app_acceptreason_") {
                    systems::applications::handle_app_reason_modal(&ctx, &i, true).await;
                } else if id.starts_with("app_denyreason_") {
                    systems::applications::handle_app_reason_modal(&ctx, &i, false).await;
                }
            }
            _ => {}
        }
    }

    // When added to a new server: snapshot it and notify the owner. (Global
    // commands already cover new guilds - no per-guild registration needed.)
    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        if is_new != Some(true) {
            return;
        }
        println!("➕ Joined guild {} ({})", guild.name, guild.id);
        systems::snapshot_rollback::snapshot_guild(&ctx, guild.id).await;
        systems::tickets::ensure_ticket_panel(&ctx, guild.id).await;
        systems::applications::ensure_application_panels(&ctx, guild.id).await;
        systems::chain_of_command::render_all_chains_of_command(&ctx, guild.id).await;

        if CONFIG.owner_dm {
            for id in common::config::BOT_OWNER_IDS.iter() {
                let Ok(raw) = id.parse::<u64>() else { continue };
                let user_id = serenity::model::id::UserId::new(raw);
                if let Ok(user) = user_id.to_user(&ctx.http).await {
                    let _ = user
                        .direct_message(
                            &ctx.http,
                            serenity::builder::CreateMessage::new().content(format!(
                                "Just got added to **{}** (`{}`). To get set up fast, run `/setup quick` over there - it'll create a mute role and the log channels for you. Then point me at your staff role with `/setup roles mod_role:@YourStaffRole` and you're good.",
                                guild.name, guild.id
                            )),
                        )
                        .await;
                }
            }
        }
    }
}

fn my_permissions(ctx: &Context, guild_id: GuildId) -> Option<Permissions> {
    let me = ctx.cache.current_user().id;
    let guild = ctx.cache.guild(guild_id)?;
    let member = guild.members.get(&me)?;
    Some(guild.member_permissions(member))
}

/// Rolling full-guild snapshots for nuke recovery.
fn spawn_snapshot_timer(ctx: Context) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(CONFIG.snapshot_interval_ms));
        interval.tick().await; // the first tick fires immediately; we already snapshotted on ready
        loop {
            interval.tick().await;
            for guild_id in ctx.cache.guilds() {
                systems::snapshot_rollback::snapshot_guild(&ctx, guild_id).await;
            }
        }
    });
}

/// Periodic sweep: trim stale tracker entries + self-defense health check.
fn spawn_sweep_timer(ctx: Context) {
    tokio::spawn(async move {
        let mut health: std::collections::HashMap<GuildId, bool> = std::collections::HashMap::new();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            systems::anti_spam::sweep();
            systems::anti_raid::sweep();
            systems::anti_nuke::sweep();

            // If I lose the permissions anti-nuke needs, alert the owner (once
            // per state change).
            for guild_id in ctx.cache.guilds() {
                let Some(perms) = my_permissions(&ctx, guild_id) else { continue };
                let ok = perms.contains(Permissions::VIEW_AUDIT_LOG)
                    && perms.contains(Permissions::BAN_MEMBERS)
                    && perms.contains(Permissions::MANAGE_ROLES);
                if health.get(&guild_id) != Some(&false) && !ok {
                    alert_owner(
                        &ctx,
                        guild_id,
                        "I've lost some permissions I really need (View Audit Log, Ban Members, or Manage Roles), which means anti-nuke could be flying blind right now. Please check my role position and permissions as soon as you can.",
                        colors::DANGER,
                        "I Need My Permissions Back",
                    )
                    .await;
                }
                health.insert(guild_id, ok);
            }
        }
    });
}

/// Resolves the moment the process is asked to stop, naming the signal that
/// did it.
///
/// SIGTERM matters as much as SIGINT here: it is what systemd, `docker stop`,
/// Kubernetes and `deploy.sh` all send. Without a handler for it the process
/// dies on the spot and the shards never disconnect cleanly.
#[cfg(unix)]
async fn wait_for_shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            // Registering SIGTERM failed, which should not happen on a normal
            // Unix host. Fall back to SIGINT alone rather than giving up on
            // graceful shutdown entirely.
            eprintln!("⚠️ couldn't listen for SIGTERM ({e}); only SIGINT will shut down cleanly.");
            tokio::signal::ctrl_c().await.ok();
            return "SIGINT";
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = term.recv()             => "SIGTERM",
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> &'static str {
    tokio::signal::ctrl_c().await.ok();
    "SIGINT"
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let _ = START_TIME.set(now_ms());

    common::db::init();
    state::run_migrations();

    if TOKEN.is_empty() {
        eprintln!("❌ DISCORD_TOKEN is not set.");
        std::process::exit(1);
    }

    let intents = serenity::model::gateway::GatewayIntents::GUILDS
        | serenity::model::gateway::GatewayIntents::GUILD_MESSAGES
        | serenity::model::gateway::GatewayIntents::GUILD_MEMBERS
        | serenity::model::gateway::GatewayIntents::GUILD_MODERATION
        | serenity::model::gateway::GatewayIntents::GUILD_WEBHOOKS
        | serenity::model::gateway::GatewayIntents::MESSAGE_CONTENT
        | serenity::model::gateway::GatewayIntents::DIRECT_MESSAGES;

    let mut client = match Client::builder(TOKEN.as_str(), intents).event_handler(Handler).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ Failed to build client: {e}");
            std::process::exit(1);
        }
    };

    // Graceful shutdown: disconnect cleanly on SIGINT/SIGTERM.
    let shard_manager = client.shard_manager.clone();
    let _ = SHARD_MANAGER.set(shard_manager.clone());
    tokio::spawn(async move {
        let signal = wait_for_shutdown_signal().await;
        println!("\n{signal} received - shutting down…");
        shard_manager.shutdown_all().await;
    });

    // The dashboard shares this process because guild state lives in memory
    // here; a separate service would keep its own copy and they would fight.
    tokio::spawn(web::serve());

    if let Err(e) = client.start_autosharded().await {
        eprintln!("❌ Client error: {e}");
    }
}
