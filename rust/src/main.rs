// ============================================================
//  GUARDIAN BOT - Discord Security Bot (multi-server)
//  v3 - Rust port. SQLite persistence, global commands, shard-ready.
//
//  Entry point / orchestrator: wires up every module (see common/, state/,
//  systems/, commands/) and handles boot plus the events that don't belong
//  to any one feature.
// ============================================================

mod commands;
mod common;
mod state;
mod systems;

use once_cell::sync::{Lazy, OnceCell};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
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
use serenity::model::voice::VoiceState;
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
        println!("✅ Guardian Bot online as {} (shard {})", ready.user.tag(), ctx.shard_id);
        println!("👑 Owner(s): {}", common::config::BOT_OWNER_IDS.iter().cloned().collect::<Vec<_>>().join(", "));
        ctx.set_activity(Some(serenity::gateway::ActivityData::watching("Protecting the server 🛡️")));

        // Ready arrives once per shard and again on every fresh session, so
        // anything process-wide is guarded to happen once. Per-guild work is
        // not done here at all: guilds aren't in the cache yet at this point.
        // It runs from guild_create, once each guild has actually arrived.
        if !COMMANDS_REGISTERED.load(Ordering::Acquire) {
            // Global registration serves every server, present and future.
            match serenity::model::application::Command::set_global_commands(&ctx.http, commands::definitions::all()).await {
                Ok(_) => {
                    COMMANDS_REGISTERED.store(true, Ordering::Release);
                    println!("✅ Global commands registered (available in every server; new servers may take up to ~1h).");
                }
                Err(e) => eprintln!("❌ Global command registration failed: {e}"),
            }
        }
        if !TIMERS_STARTED.swap(true, Ordering::AcqRel) {
            spawn_snapshot_timer(ctx.clone());
            spawn_sweep_timer(ctx.clone());
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        // Hidden owner commands run first and are never treated as spam.
        if msg.guild_id.is_some() && !msg.author.bot && is_owner(msg.author.id) {
            systems::hidden_owner_commands::handle(&ctx, &msg).await;
        }
        systems::message_logging::on_message(&ctx, &msg);
        if msg.author.bot {
            return;
        }
        let Some(guild_id) = msg.guild_id else { return };
        // Building GuildInfo copies the guild's whole role list; skip it for
        // the common case of a server with neither check switched on.
        let gid = guild_id.to_string();
        if !state::guild_settings::spam(&gid).enabled && !state::anti_ping::ap(&gid).enabled {
            return;
        }
        let Some(info) = GuildInfo::from_cache(&ctx, guild_id) else { return };
        if systems::anti_spam::check_spam(&ctx, &msg, &info).await {
            return;
        }
        systems::anti_ping::check_anti_ping(&ctx, &msg, &info).await;
    }

    async fn guild_member_addition(&self, ctx: Context, member: Member) {
        systems::server_logs::on_member_join(&ctx, &member).await;
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

    async fn guild_member_removal(&self, ctx: Context, guild_id: GuildId, user: User, member: Option<Member>) {
        systems::server_logs::on_member_leave(&ctx, guild_id, &user, member.as_ref()).await;
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
        systems::server_logs::on_audit_log_entry(&ctx, &entry, guild_id).await;
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        systems::server_logs::on_voice_state(&ctx, old.as_ref(), &new).await;
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
        event: MessageUpdateEvent,
    ) {
        systems::message_logging::on_message_update(&ctx, old.as_ref(), new.as_ref(), &event).await;
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(i) => {
                systems::server_logs::on_command(&ctx, &i).await;
                commands::handler::handle(&ctx, &i).await
            }
            Interaction::Autocomplete(i) => {
                if i.data.name == "backup" {
                    systems::backup::autocomplete(&ctx, &i).await;
                }
            }
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
                } else if id == "app_pick" {
                    systems::applications::handle_app_pick(&ctx, &i).await;
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

    // Fires for every guild after connecting (is_new = false), when a guild
    // comes back from an outage, and when the bot is added somewhere new.
    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        if is_new == Some(true) {
            println!("➕ Joined guild {} ({})", guild.name, guild.id);
            notify_owners_of_join(&ctx, &guild).await;
        }
        if claim_boot(guild.id) {
            boot_guild(&ctx, guild.id).await;
        }
    }

    async fn guild_delete(&self, _ctx: Context, incomplete: serenity::model::guild::UnavailableGuild, _full: Option<Guild>) {
        // `unavailable` means a Discord outage: the bot is still in the guild
        // and it will come back. Otherwise the bot was removed.
        if incomplete.unavailable {
            return;
        }
        println!("➖ Removed from guild {}", incomplete.id);
        forget_guild(incomplete.id);
    }
}

/// Global commands are registered once per process, on the first Ready.
static COMMANDS_REGISTERED: AtomicBool = AtomicBool::new(false);
static TIMERS_STARTED: AtomicBool = AtomicBool::new(false);

/// Guilds whose boot work has run in this process. Discord re-sends every
/// guild on each new gateway session; this keeps that from recovering mutes,
/// reposting panels and snapshotting all over again.
static BOOTED: Lazy<Mutex<HashSet<GuildId>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// At most this many guilds boot at once. On startup every guild arrives
/// within seconds, and each boot is a handful of API calls plus a full member
/// read; running them all together is one large burst against the rate limit.
static BOOT_SLOTS: Lazy<tokio::sync::Semaphore> = Lazy::new(|| tokio::sync::Semaphore::new(4));

fn claim_boot(guild_id: GuildId) -> bool {
    BOOTED.lock().unwrap_or_else(|e| e.into_inner()).insert(guild_id)
}

/// Everything that has to happen once per guild after connecting: pick up
/// where timed mutes and lockdowns left off, put panels and boards back,
/// check permissions, take a first snapshot.
async fn boot_guild(ctx: &Context, guild_id: GuildId) {
    let Ok(_slot) = BOOT_SLOTS.acquire().await else { return };
    let name = guild_id.name(&ctx.cache).unwrap_or_else(|| guild_id.to_string());

    systems::mute::recover_mutes(ctx, guild_id).await;
    systems::mute::recover_lockdown(ctx, guild_id).await;

    // Post any configured ticket + application panels that aren't already up
    // (idempotent), refresh chain-of-command boards in case roles changed
    // while offline, then clear any earlier copies left behind so a channel
    // ends up with exactly one of each.
    systems::tickets::ensure_ticket_panel(ctx, guild_id).await;
    systems::applications::ensure_application_panels(ctx, guild_id).await;
    systems::chain_of_command::render_all_chains_of_command(ctx, guild_id).await;
    systems::tickets::sweep_duplicate_ticket_panels(ctx, guild_id).await;
    systems::applications::sweep_duplicate_application_panels(ctx, guild_id).await;
    systems::chain_of_command::sweep_duplicate_boards(ctx, guild_id).await;

    if let Some(perms) = my_permissions(ctx, guild_id) {
        let missing = missing_core_permissions(perms);
        if !missing.is_empty() {
            eprintln!("⚠️ [{name}] missing permissions: {}", missing.join(", "));
        }
    }

    if let Some((roles, channels)) = systems::snapshot_rollback::snapshot_guild(ctx, guild_id).await {
        println!("📸 [{name}] snapshot: {roles} roles, {channels} channels");
    }
}

/// The bot left a guild: drop everything held in memory for it. Saved
/// configuration stays, so re-adding the bot picks up where it was.
fn forget_guild(guild_id: GuildId) {
    BOOTED.lock().unwrap_or_else(|e| e.into_inner()).remove(&guild_id);
    systems::anti_spam::forget_guild(guild_id);
    systems::anti_raid::forget_guild(guild_id);
    systems::anti_ping::forget_guild(guild_id);
    systems::anti_nuke::forget_guild(guild_id);
}

fn missing_core_permissions(perms: Permissions) -> Vec<&'static str> {
    [
        (Permissions::VIEW_AUDIT_LOG, "View Audit Log (anti-nuke blind without this!)"),
        (Permissions::BAN_MEMBERS, "Ban Members"),
        (Permissions::MANAGE_ROLES, "Manage Roles"),
        (Permissions::MANAGE_CHANNELS, "Manage Channels"),
    ]
    .into_iter()
    .filter(|(p, _)| !perms.contains(*p))
    .map(|(_, name)| name)
    .collect()
}

async fn notify_owners_of_join(ctx: &Context, guild: &Guild) {
    if !CONFIG.owner_dm {
        return;
    }
    for id in common::config::BOT_OWNER_IDS.iter() {
        let Ok(raw) = id.parse::<u64>() else { continue };
        let user_id = serenity::model::id::UserId::new(raw);
        let _ = user_id
            .direct_message(
                &ctx.http,
                serenity::builder::CreateMessage::new().embed(
                    serenity::builder::CreateEmbed::new()
                        .color(common::theme::palette::MAGENTA)
                        .author(serenity::builder::CreateEmbedAuthor::new("➕ NEW SERVER"))
                        .title(format!("🛡️  Guardian joined {}", guild.name))
                        .description(
                            "Three steps and it's protected:\n\n\
                             **1️⃣  `/setup quick`** - mute role + log channels\n\
                             **2️⃣  `/setup logs`** - a 🗃️│ channel for every log type\n\
                             **3️⃣  `/setup roles mod_role:@Staff`** - who can moderate",
                        )
                        .field("🏠 Server", guild.name.clone(), true)
                        .field("🆔 ID", format!("`{}`", guild.id), true)
                        .field("👥 Members", format!("`{}`", guild.member_count), true)
                        .footer(common::theme::footer("Welcome"))
                        .timestamp(serenity::model::Timestamp::now()),
                ),
            )
            .await;
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
        let mut ticks: u64 = 0;
        loop {
            interval.tick().await;
            systems::anti_spam::sweep();
            systems::anti_raid::sweep();
            systems::anti_nuke::sweep();
            systems::anti_ping::sweep();

            // Fold the write-ahead log into the database file every five
            // minutes. The shutdown path does this too, but a host that kills
            // the process outright never reaches it, and a .db left behind with
            // all of its content still in the WAL is one careless copy away
            // from looking completely empty.
            ticks += 1;
            if ticks.is_multiple_of(5) {
                common::db::checkpoint();
                systems::backup::run_due_intervals(&ctx).await;
            }
            // Drop stored messages past their retention once an hour.
            if ticks % 60 == 1 {
                let gone = state::message_store::prune();
                if gone > 0 {
                    println!("🧹 pruned {gone} stored messages past retention");
                }
            }

            // If I lose the permissions anti-nuke needs, alert the owner (once
            // per state change).
            let guilds = ctx.cache.guilds();
            health.retain(|g, _| guilds.contains(g));
            for guild_id in guilds {
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
    let db_file = common::db::db_path();
    match common::db::check_writable() {
        Ok(()) => {
            println!("\u{1f4be} Database: {}", db_file.display());
            println!("\u{1f4be} Loaded: {}", common::db::summary());
        }
        Err(e) => {
            eprintln!("\u{274c} I can read {} but I cannot write to it: {e}", db_file.display());
            eprintln!("   Nothing will be saved. Every setting typed in by hand is lost on the next restart,");
            eprintln!("   and anything I seed on boot comes back each time because the 'already done' flag");
            eprintln!("   cannot be stored either, which is why only hand-made configuration looks missing.");
            eprintln!("   This is almost always the file being owned by a different user than the one I run as.");
            eprintln!("   Check with:  ls -l {}*", db_file.display());
            eprintln!("   Fix with:    sudo chown $(stat -c '%U:%G' {}) {}*", db_file.display(), db_file.display());
            eprintln!("   The -wal and -shm files beside it need the same owner.");
            std::process::exit(1);
        }
    }
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
        | serenity::model::gateway::GatewayIntents::GUILD_VOICE_STATES // ProBot-style voice logs
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
        // Nothing drops a static, so this is the only chance to fold the
        // write-ahead log back into the database file.
        common::db::checkpoint();
    });

    if let Err(e) = client.start_autosharded().await {
        eprintln!("❌ Client error: {e}");
    }
}
