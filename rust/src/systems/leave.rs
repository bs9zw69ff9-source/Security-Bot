//! `/leave`: bot-owner-only. Lists every server the bot is in as a dropdown,
//! confirms, then leaves whichever one was picked.
//!
//! The whole flow only ever reads the cache of every server the bot is in -
//! never the server (if any) the command was run from - so it works the same
//! whether it's run in a server, in the bot's own DMs, or in a group chat.

use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseMessage,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, EditInteractionResponse,
};
use serenity::client::Context;
use serenity::collector::ComponentInteractionCollector;
use serenity::model::application::{ButtonStyle, CommandInteraction, ComponentInteractionDataKind};
use serenity::model::id::GuildId;
use std::time::Duration;

use crate::common::permissions::is_owner;
use crate::common::theme::{self, Tone};

/// How long the owner has to pick a server from the dropdown.
const SELECT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long they then have to confirm the one they picked.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

const CONFIRM: &str = "leave_confirm";
const CANCEL: &str = "leave_cancel";

/// Select menus carry at most 25 options, and a message at most 5 action
/// rows, so one listing can offer at most this many servers before some are
/// left out. The biggest servers are shown first, since those are the most
/// likely picks and the easiest to recognise.
const MAX_OPTIONS: usize = 25;
const MAX_ROWS: usize = 5;
const MAX_SHOWN: usize = MAX_OPTIONS * MAX_ROWS;

/// One server, reduced to what the dropdown needs.
#[derive(Clone)]
struct Listed {
    id: GuildId,
    name: String,
    members: u64,
}

/// `/leave`: owner only. Checked again here even though every caller already
/// gates on it - leaving a server is as consequential as anything this bot
/// does, so it gets the same defense-in-depth as `!wipe`.
pub async fn run(ctx: &Context, i: &CommandInteraction) {
    if !is_owner(i.user.id) {
        return;
    }

    let guilds: Vec<Listed> = ctx
        .cache
        .guilds()
        .into_iter()
        .filter_map(|gid| ctx.cache.guild(gid).map(|g| Listed { id: gid, name: g.name.to_string(), members: g.member_count }))
        .collect();
    let total = guilds.len();
    if total == 0 {
        let _ = i
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .embed(theme::card(Tone::Info, None, "I'm not in any servers."))
                        .ephemeral(true),
                ),
            )
            .await;
        return;
    }

    let (rows, dropped) = select_rows(guilds);
    let _ = i
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new().embed(intro_embed(total, dropped)).components(rows).ephemeral(true),
            ),
        )
        .await;
    let Ok(prompt) = i.get_response(&ctx.http).await else { return };
    let prompt_id = prompt.id;
    let author = i.user.id;

    // Step 1: which server.
    let selected = ComponentInteractionCollector::new(&ctx.shard)
        .timeout(SELECT_TIMEOUT)
        .filter(move |c| c.message.id == prompt_id && c.user.id == author)
        .next()
        .await;
    let Some(select) = selected else {
        let card = theme::card(Tone::Info, None, "Nothing picked in time. Run `/leave` again if you still want to.");
        let _ = i.edit_response(&ctx.http, EditInteractionResponse::new().embed(card).components(vec![])).await;
        return;
    };
    let ComponentInteractionDataKind::StringSelect { values } = &select.data.kind else {
        let _ = select.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
        return;
    };
    let Some(gid) = values.first().and_then(|v| v.parse::<u64>().ok()).map(GuildId::new) else {
        let _ = select.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
        return;
    };
    let Some(name) = ctx.cache.guild(gid).map(|g| g.name.to_string()) else {
        let card = theme::card(Tone::Error, None, "I'm not in that server anymore.");
        let _ = select
            .create_response(
                &ctx.http,
                CreateInteractionResponse::UpdateMessage(CreateInteractionResponseMessage::new().embed(card).components(vec![])),
            )
            .await;
        return;
    };

    // Step 2: confirm. Nothing happens on the pick alone - the owner has to
    // click through, same as a wipe.
    let confirm_body = format!(
        "Leave **{name}** (`{gid}`)? I won't be able to get back in without a fresh invite.\n\n\
         Click **Leave** within 60s to confirm, or **Cancel**."
    );
    let buttons = CreateActionRow::Buttons(vec![
        CreateButton::new(CONFIRM).label("Leave").style(ButtonStyle::Danger),
        CreateButton::new(CANCEL).label("Cancel").style(ButtonStyle::Secondary),
    ]);
    let confirm_card = theme::card(Tone::Warning, Some("🚪  Confirm leaving"), confirm_body);
    let _ = select
        .create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(CreateInteractionResponseMessage::new().embed(confirm_card).components(vec![buttons])),
        )
        .await;

    let confirmed = ComponentInteractionCollector::new(&ctx.shard)
        .timeout(CONFIRM_TIMEOUT)
        .filter(move |c| c.message.id == prompt_id && c.user.id == author && (c.data.custom_id == CONFIRM || c.data.custom_id == CANCEL))
        .next()
        .await;
    let Some(confirm) = confirmed else {
        let card = theme::card(Tone::Info, None, format!("Timed out - still in **{name}**."));
        let _ = select.edit_response(&ctx.http, EditInteractionResponse::new().embed(card).components(vec![])).await;
        return;
    };
    if confirm.data.custom_id == CANCEL {
        let card = theme::card(Tone::Info, None, format!("Cancelled - still in **{name}**."));
        let _ = confirm
            .create_response(
                &ctx.http,
                CreateInteractionResponse::UpdateMessage(CreateInteractionResponseMessage::new().embed(card).components(vec![])),
            )
            .await;
        return;
    }

    // Confirmed. Defer: leaving is one more HTTP call and Discord only gives a
    // component response 3 seconds, then edit once the outcome is known.
    let _ = confirm.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await;
    crate::common::db::append_forensic(
        &gid.to_string(),
        "owner_leave",
        serde_json::json!({ "by": author.to_string(), "name": name }),
    );
    let card = match gid.leave(&ctx.http).await {
        Ok(()) => {
            println!("👋 Left {name} ({gid}) - requested by {author}");
            theme::card(Tone::Success, Some("🚪  Left the server"), format!("Done - I've left **{name}**."))
        }
        Err(e) => theme::card(Tone::Error, None, format!("I couldn't leave **{name}**: {e}")),
    };
    let _ = confirm.edit_response(&ctx.http, EditInteractionResponse::new().embed(card).components(vec![])).await;
}

/// The dropdown rows for up to MAX_SHOWN servers (biggest first), and how
/// many didn't fit. Pure, so the chunking and ordering are testable without a
/// live cache.
fn select_rows(mut guilds: Vec<Listed>) -> (Vec<CreateActionRow>, usize) {
    guilds.sort_by(|a, b| b.members.cmp(&a.members).then(a.id.cmp(&b.id)));
    let dropped = guilds.len().saturating_sub(MAX_SHOWN);
    guilds.truncate(MAX_SHOWN);

    let rows = guilds
        .chunks(MAX_OPTIONS)
        .enumerate()
        .map(|(idx, chunk)| {
            let options = chunk
                .iter()
                .map(|g| {
                    CreateSelectMenuOption::new(truncate(&g.name, 100), g.id.to_string())
                        .description(truncate(&format!("{} members · ID {}", thousands(g.members), g.id), 100))
                })
                .collect();
            CreateActionRow::SelectMenu(
                CreateSelectMenu::new(format!("leave_select_{idx}"), CreateSelectMenuKind::String { options })
                    .placeholder("Pick a server to leave…")
                    .min_values(1)
                    .max_values(1),
            )
        })
        .collect();
    (rows, dropped)
}

/// The `/leave` intro card: how many servers, and how many were left off the
/// list, if any - never a silent cap.
fn intro_embed(total: usize, dropped: usize) -> CreateEmbed {
    let mut body = format!(
        "I'm in **{total}** server{}. Pick one below to leave it - I'll ask you to confirm first.",
        plural(total)
    );
    if dropped > 0 {
        body.push_str(&format!(
            "\n\n*Showing the {} biggest; {dropped} more aren't listed here. Leave some to bring the rest into view.*",
            total - dropped
        ));
    }
    theme::card(Tone::Info, Some("🚪  Leave a server"), body)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(id: u64, name: &str, members: u64) -> Listed {
        Listed { id: GuildId::new(id), name: name.into(), members }
    }

    /// The biggest servers are easiest to recognise, so they come first.
    #[test]
    fn biggest_servers_come_first() {
        let guilds = vec![listed(1, "Small", 5), listed(2, "Big", 500), listed(3, "Medium", 50)];
        let (rows, dropped) = select_rows(guilds);
        assert_eq!(dropped, 0);
        let json = serde_json::to_string(&rows).unwrap();
        let big = json.find("\"Big\"").unwrap();
        let medium = json.find("\"Medium\"").unwrap();
        let small = json.find("\"Small\"").unwrap();
        assert!(big < medium && medium < small, "expected Big, Medium, Small in that order: {json}");
    }

    /// Equal member counts still need a deterministic order.
    #[test]
    fn ties_break_by_id() {
        let guilds = vec![listed(20, "B", 10), listed(10, "A", 10)];
        let (rows, _) = select_rows(guilds);
        let json = serde_json::to_string(&rows).unwrap();
        assert!(json.find("\"A\"").unwrap() < json.find("\"B\"").unwrap(), "{json}");
    }

    /// Discord allows 25 options per select menu, so more than that needs a
    /// second row.
    #[test]
    fn more_than_one_selects_worth_splits_into_rows() {
        let guilds: Vec<Listed> = (0..30).map(|n| listed(1000 + n, &format!("Server {n}"), 100 - n)).collect();
        let (rows, dropped) = select_rows(guilds);
        assert_eq!(dropped, 0);
        assert_eq!(rows.len(), 2, "25 options per select, 30 servers needs two rows");
    }

    /// A deployment in more servers than fit across five selects still gets a
    /// listing - the extra ones are left out and counted, never silently lost.
    #[test]
    fn servers_past_the_limit_are_counted_not_lost() {
        let guilds: Vec<Listed> = (0..140).map(|n| listed(2000 + n, &format!("Server {n}"), 200 - n)).collect();
        let (rows, dropped) = select_rows(guilds);
        assert_eq!(rows.len(), MAX_ROWS);
        assert_eq!(dropped, 140 - MAX_SHOWN);
    }

    /// The dropdown picks a server by its id, not its (editable, collidable) name.
    #[test]
    fn the_option_value_is_the_guild_id() {
        let (rows, _) = select_rows(vec![listed(1_234_567_890, "Test", 1)]);
        let json = serde_json::to_string(&rows).unwrap();
        assert!(json.contains("\"value\":\"1234567890\""), "{json}");
    }

    #[test]
    fn the_intro_names_the_count_and_any_drop() {
        let shown = serde_json::to_string(&intro_embed(3, 0)).unwrap();
        assert!(shown.contains("**3**"), "{shown}");
        assert!(!shown.contains("Showing the"), "nothing to say when nothing was dropped: {shown}");

        let truncated = serde_json::to_string(&intro_embed(140, 15)).unwrap();
        assert!(truncated.contains("**140**") && truncated.contains("15 more"), "{truncated}");
    }
}
