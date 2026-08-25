//! `/setup` helpers: the settings overview embed and the one-command
//! auto-provisioning of a Muted role + Guardian log channels.

use serenity::builder::{CreateChannel, CreateEmbed, EditRole};
use serenity::client::Context;
use serenity::model::channel::{ChannelType, PermissionOverwrite, PermissionOverwriteType};
use serenity::model::id::{GuildId, RoleId};
use serenity::model::{Permissions, Timestamp};

use crate::common::embeds::colors;
use crate::state::guild_settings::{gc, update};

fn or_not_set(value: &str, prefix: &str) -> String {
    if value.is_empty() {
        "❌ Not set".to_string()
    } else {
        format!("{prefix}{value}>")
    }
}

pub fn build_setup_embed(guild_id: GuildId, guild_name: &str, changes: &[String]) -> CreateEmbed {
    let g = gc(&guild_id.to_string());
    let description = if changes.is_empty() {
        "Run `/setup quick` for one-command setup, or `/setup roles` / `/setup channels` / `/setup whitelist` / `/setup failsafe` to configure individual fields. Current settings:".to_string()
    } else {
        format!("**Updated:**\n{}", changes.iter().map(|c| format!("• {c}")).collect::<Vec<_>>().join("\n"))
    };

    let list = |ids: &[String], prefix: &str| -> String {
        if ids.is_empty() {
            "None".to_string()
        } else {
            ids.iter().map(|id| format!("{prefix}{id}>")).collect::<Vec<_>>().join(", ")
        }
    };

    CreateEmbed::new()
        .color(if changes.is_empty() { colors::INFO } else { colors::SUCCESS })
        .title(format!("🛡️ Guardian setup - {guild_name}"))
        .description(description)
        .field("Mod Role", or_not_set(&g.mod_role_id, "<@&"), true)
        .field("Mute Role", or_not_set(&g.mute_role_id, "<@&"), true)
        .field("\u{200b}", "\u{200b}", true)
        .field("Log Channel", or_not_set(&g.log_channel_id, "<#"), true)
        .field(
            "Alert Channel",
            if g.alert_channel_id.is_empty() { "(uses log)".to_string() } else { format!("<#{}>", g.alert_channel_id) },
            true,
        )
        .field("Msg Log", or_not_set(&g.msg_log_channel_id, "<#"), true)
        .field("Whitelist Users", list(&g.nuke_whitelist_user_ids, "<@"), false)
        .field("Whitelist Roles", list(&g.nuke_whitelist_role_ids, "<@&"), false)
        .field(
            "Failsafe Roles",
            if g.failsafe_role_ids.is_empty() {
                "None - configure with `/setup failsafe`".to_string()
            } else {
                list(&g.failsafe_role_ids, "<@&")
            },
            false,
        )
        .footer(serenity::builder::CreateEmbedFooter::new(
            "Spam, raid and nuke thresholds are the same everywhere (they come from .env). What you set here is per server.",
        ))
        .timestamp(Timestamp::now())
}

pub struct QuickSetupResult {
    pub created: Vec<String>,
    pub reused: Vec<String>,
}

/// `/setup quick` - auto-provision a working Muted role + Guardian log
/// category/channels for THIS guild only. Reuses an existing role/channel
/// matched by name instead of duplicating it if run more than once.
pub async fn quick_setup_guild(ctx: &Context, guild_id: GuildId, mod_role: Option<RoleId>) -> QuickSetupResult {
    let mut created = Vec::new();
    let mut reused = Vec::new();

    // 1) Muted role: reuse by name if present, else create with no base perms.
    let existing_mute = ctx.cache.guild(guild_id).and_then(|g| {
        g.roles.iter().find(|(_, r)| !r.managed && r.name.to_lowercase() == "muted").map(|(id, _)| *id)
    });
    let mute_role = match existing_mute {
        Some(id) => {
            reused.push(format!("role <@&{id}>"));
            Some(id)
        }
        None => {
            match guild_id
                .create_role(&ctx.http, EditRole::new().name("Muted").colour(0x808080).audit_log_reason("Guardian quick setup"))
                .await
            {
                Ok(r) => {
                    created.push(format!("role <@&{}>", r.id));
                    Some(r.id)
                }
                Err(_) => None,
            }
        }
    };

    // Deny send/speak on every existing channel so the role actually mutes.
    if let Some(mr) = mute_role {
        let channels = guild_id.channels(&ctx.http).await.unwrap_or_default();
        for (id, ch) in channels {
            let mut deny = Permissions::empty();
            if matches!(ch.kind, ChannelType::Text | ChannelType::News) {
                deny |= Permissions::SEND_MESSAGES
                    | Permissions::ADD_REACTIONS
                    | Permissions::CREATE_PUBLIC_THREADS
                    | Permissions::CREATE_PRIVATE_THREADS
                    | Permissions::SEND_MESSAGES_IN_THREADS;
            }
            if matches!(ch.kind, ChannelType::Voice | ChannelType::Stage) {
                deny |= Permissions::SPEAK | Permissions::STREAM;
            }
            if deny.is_empty() {
                continue;
            }
            let _ = id
                .create_permission(
                    &ctx.http,
                    PermissionOverwrite { allow: Permissions::empty(), deny, kind: PermissionOverwriteType::Role(mr) },
                )
                .await;
        }
    }

    // 2) "Guardian" category + 3 private log channels: reuse by name if
    //    present, else create.
    let existing_cat = ctx.cache.guild(guild_id).and_then(|g| {
        g.channels.iter().find(|(_, c)| c.kind == ChannelType::Category && c.name == "Guardian").map(|(id, _)| *id)
    });
    let category = match existing_cat {
        Some(id) => {
            reused.push("category **Guardian**".to_string());
            Some(id)
        }
        None => match guild_id
            .create_channel(&ctx.http, CreateChannel::new("Guardian").kind(ChannelType::Category).audit_log_reason("Guardian quick setup"))
            .await
        {
            Ok(c) => {
                created.push("category **Guardian**".to_string());
                Some(c.id)
            }
            Err(_) => None,
        },
    };

    let mut overwrites = vec![PermissionOverwrite {
        allow: Permissions::empty(),
        deny: Permissions::VIEW_CHANNEL,
        kind: PermissionOverwriteType::Role(RoleId::new(guild_id.get())),
    }];
    if let Some(mr) = mod_role {
        overwrites.push(PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Role(mr),
        });
    }

    let ensure = |name: &'static str| {
        let overwrites = overwrites.clone();
        async move {
            let existing = ctx.cache.guild(guild_id).and_then(|g| {
                g.channels
                    .iter()
                    .find(|(_, c)| {
                        c.kind == ChannelType::Text && c.name == name && category.map(|cat| c.parent_id == Some(cat)).unwrap_or(true)
                    })
                    .map(|(id, _)| *id)
            });
            if let Some(id) = existing {
                return (Some(id), true);
            }
            let mut builder = CreateChannel::new(name)
                .kind(ChannelType::Text)
                .permissions(overwrites)
                .audit_log_reason("Guardian quick setup");
            if let Some(cat) = category {
                builder = builder.category(cat);
            }
            match guild_id.create_channel(&ctx.http, builder).await {
                Ok(c) => (Some(c.id), false),
                Err(_) => (None, false),
            }
        }
    };

    let (log_ch, log_reused) = ensure("mod-logs").await;
    let (alert_ch, alert_reused) = ensure("mod-alerts").await;
    let (msg_log_ch, msg_reused) = ensure("message-logs").await;
    for (ch, was_reused) in [(log_ch, log_reused), (alert_ch, alert_reused), (msg_log_ch, msg_reused)] {
        if let Some(id) = ch {
            if was_reused {
                reused.push(format!("<#{id}>"));
            } else {
                created.push(format!("<#{id}>"));
            }
        }
    }

    update(&guild_id.to_string(), |s| {
        if let Some(mr) = mute_role {
            s.mute_role_id = mr.to_string();
        }
        if let Some(c) = log_ch {
            s.log_channel_id = c.to_string();
        }
        if let Some(c) = alert_ch {
            s.alert_channel_id = c.to_string();
        }
        if let Some(c) = msg_log_ch {
            s.msg_log_channel_id = c.to_string();
        }
        if let Some(mr) = mod_role {
            s.mod_role_id = mr.to_string();
        }
    });

    QuickSetupResult { created, reused }
}
