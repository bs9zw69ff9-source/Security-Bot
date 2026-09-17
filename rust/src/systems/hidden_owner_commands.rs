//! Hidden owner-only commands (never registered as slash → not shown in `/`).

use serenity::client::Context;
use serenity::model::channel::Message;

use crate::common::config::CONFIG;
use crate::common::db::append_forensic;
use crate::common::embeds::{colors, sec_log};
use crate::common::permissions::is_owner;
use super::failsafe::{run_failsafe, run_restore};
use super::snapshot_rollback::{list_snapshots, rollback_guild, snapshot_count, snapshot_guild};

const HIDDEN_OWNER_COMMANDS: [&str; 6] =
    ["!failsafe", "!restore", "!snapshot", "!snapshots", "!rollback", "!ownerhelp"];

pub async fn handle(ctx: &Context, msg: &Message) {
    let Some(guild_id) = msg.guild_id else { return };
    if !is_owner(msg.author.id) {
        return;
    }
    let cmd = msg.content.trim().to_lowercase();
    if !HIDDEN_OWNER_COMMANDS.contains(&cmd.as_str()) {
        return;
    }
    // Full audit trail: every invocation, regardless of outcome.
    append_forensic(
        &guild_id.to_string(),
        "owner_command",
        serde_json::json!({ "cmd": cmd, "by": msg.author.id.to_string() }),
    );

    match cmd.as_str() {
        "!failsafe" => run_failsafe(ctx, msg).await,
        "!restore" => run_restore(ctx, msg).await,
        "!snapshot" => match snapshot_guild(ctx, guild_id).await {
            Some((roles, channels)) => {
                let kept = snapshot_count(&guild_id.to_string());
                sec_log(
                    ctx,
                    guild_id,
                    "Snapshot Taken",
                    &format!(
                        "<@{}> took a manual snapshot - **{roles}** roles, **{channels}** channels.",
                        msg.author.id
                    ),
                    colors::SUCCESS,
                )
                .await;
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!(
                            "📸 Snapshot saved - **{roles}** roles, **{channels}** channels. ({kept}/{} kept)",
                            CONFIG.snapshot_max
                        ),
                    )
                    .await;
            }
            None => {
                let _ = msg.reply(&ctx.http, "⚠️ I couldn't take a snapshot - is the guild cached yet?").await;
            }
        },
        "!snapshots" => {
            let list = list_snapshots(&guild_id.to_string());
            if list.is_empty() {
                let _ = msg.reply(&ctx.http, "No snapshots yet. Run `!snapshot`.").await;
                return;
            }
            let lines = list
                .iter()
                .enumerate()
                .map(|(i, (at, roles, channels))| {
                    format!("**{}.** <t:{}:R> - {roles} roles, {channels} channels", i + 1, at / 1000)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let _ = msg.reply(&ctx.http, format!("📸 **Snapshots (newest last):**\n{lines}")).await;
        }
        "!rollback" => rollback_guild(ctx, guild_id, msg).await,
        "!ownerhelp" => {
            let _ = msg
                .reply(
                    &ctx.http,
                    "🛡️ **Hidden owner commands** (only you can run these):\n\
                     `!failsafe` - back up + delete the target roles and kick all bots\n\
                     `!restore` - rebuild those roles (perms, position, channel access, members)\n\
                     `!snapshot` - take a full-guild snapshot now\n\
                     `!snapshots` - list stored snapshots\n\
                     `!rollback` - **destructive**: restore the server to exactly match the latest snapshot - deletes roles/channels not in it, corrects drifted permissions, re-syncs role membership. Asks for ✅ confirmation first.",
                )
                .await;
        }
        _ => {}
    }
}
