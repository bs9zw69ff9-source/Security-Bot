//! `/serverinfo`: the server's details at a glance, with a View Roles button
//! that lists every role from the top of the hierarchy down. Both replies are
//! ephemeral, seen only by whoever asked.

use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseMessage,
};
use serenity::client::Context;
use serenity::model::application::{ButtonStyle, ComponentInteraction};
use serenity::model::channel::ChannelType;
use serenity::model::guild::{PremiumTier, VerificationLevel};
use serenity::model::id::{GuildId, RoleId};

use crate::common::theme::palette;

/// The View Roles button's custom id.
pub const VIEW_ROLES_ID: &str = "serverinfo_roles";

/// Discord allows 4096 characters per embed description, and 6000 across every
/// embed in a message; these leave room for the titles and the "and N more" line.
const PAGE_LIMIT: usize = 4000;
const MESSAGE_LIMIT: usize = 5600;
/// Embeds per message.
const MAX_PAGES: usize = 10;

/// The server info card, from the cache. `None` while the server is still
/// arriving in the cache.
pub fn serverinfo_embed(ctx: &Context, guild_id: GuildId) -> Option<CreateEmbed> {
    let g = ctx.cache.guild(guild_id)?;
    let created = guild_id.created_at().unix_timestamp();

    let (mut text, mut voice, mut categories) = (0usize, 0usize, 0usize);
    for c in g.channels.values() {
        match c.kind {
            ChannelType::Category => categories += 1,
            ChannelType::Voice | ChannelType::Stage => voice += 1,
            _ => text += 1,
        }
    }
    let boosts = g.premium_subscription_count.unwrap_or(0);
    let level = match g.premium_tier {
        PremiumTier::Tier1 => "Level 1",
        PremiumTier::Tier2 => "Level 2",
        PremiumTier::Tier3 => "Level 3",
        _ => "no level yet",
    };
    // Every role but @everyone, which every member has.
    let roles = g.roles.keys().filter(|r| r.get() != guild_id.get()).count();

    let mut e = CreateEmbed::new()
        .color(palette::AZURE)
        .title(g.name.clone())
        .field("🆔 Server ID", format!("`{guild_id}`"), true)
        .field("👑 Owner", format!("<@{}>", g.owner_id), true)
        .field("👥 Members", thousands(g.member_count), true)
        .field("📆 Created", format!("<t:{created}:F> (<t:{created}:R>)"), false)
        .field("💬 Channels", format!("{text} text · {voice} voice · {categories} categories"), true)
        .field("🚀 Boosts", format!("{boosts} ({level})"), true)
        .field("🛡️ Verification", verification(g.verification_level), true)
        .field("😀 Emojis & stickers", format!("{} emojis · {} stickers", g.emojis.len(), g.stickers.len()), true)
        .field("🔐 Roles", format!("{roles} - tap **View Roles** for the hierarchy"), true);
    if let Some(icon) = g.icon_url() {
        e = e.thumbnail(icon);
    }
    if let Some(banner) = g.banner_url() {
        e = e.image(banner);
    }
    if let Some(d) = g.description.as_ref().filter(|d| !d.trim().is_empty()) {
        e = e.description(d.clone());
    }
    Some(e)
}

/// The View Roles button under the server info card.
pub fn view_roles_row() -> CreateActionRow {
    CreateActionRow::Buttons(vec![CreateButton::new(VIEW_ROLES_ID).label("View Roles").emoji('🔐').style(ButtonStyle::Secondary)])
}

/// View Roles: every role from the top of the hierarchy down, just for the
/// person who clicked.
pub async fn handle_view_roles(ctx: &Context, i: &ComponentInteraction) {
    let reply = |msg: CreateInteractionResponseMessage| CreateInteractionResponse::Message(msg.ephemeral(true));
    let Some(guild_id) = i.guild_id else { return };
    let snapshot = ctx.cache.guild(guild_id).map(|g| {
        let rows: Vec<(RoleId, i64, bool)> = g.roles.values().map(|r| (r.id, i64::from(r.position), r.managed)).collect();
        (g.name.clone(), rows)
    });
    let Some((name, rows)) = snapshot else {
        let _ = i
            .create_response(&ctx.http, reply(CreateInteractionResponseMessage::new().content("I'm still loading this server's details. Try again in a moment.")))
            .await;
        return;
    };

    let lines = hierarchy(rows, RoleId::new(guild_id.get()));
    let total = lines.len();
    let (pages, dropped) = pack_pages(&lines);
    let embeds: Vec<CreateEmbed> = pages
        .into_iter()
        .enumerate()
        .map(|(idx, mut page)| {
            if idx == 0 && dropped > 0 {
                // Said up front, where it can't be missed at the end of a long list.
                page = format!("*Showing the top {} of {total}.*\n{page}", total - dropped);
            }
            let e = CreateEmbed::new().color(palette::AZURE).description(page);
            if idx == 0 { e.title(format!("🔐 Roles in {name} ({total})")) } else { e }
        })
        .collect();
    let _ = i.create_response(&ctx.http, reply(CreateInteractionResponseMessage::new().embeds(embeds))).await;
}

/// Roles highest first, one numbered line each, @everyone last. Pure, so the
/// ordering is testable. Equal positions fall back to the id, as Discord does.
fn hierarchy(mut rows: Vec<(RoleId, i64, bool)>, everyone: RoleId) -> Vec<String> {
    rows.retain(|(id, _, _)| *id != everyone);
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut lines: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(n, (id, _, managed))| format!("`{}.` <@&{id}>{}", n + 1, if *managed { " · bot" } else { "" }))
        .collect();
    lines.push(format!("`{}.` @everyone", lines.len() + 1));
    lines
}

/// Split lines into embed descriptions within Discord's limits. Returns the
/// pages and how many lines didn't fit (always the lowest roles).
fn pack_pages(lines: &[String]) -> (Vec<String>, usize) {
    let mut pages: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        let cost = line.chars().count() + 1;
        if used + cost > MESSAGE_LIMIT {
            if !current.is_empty() {
                pages.push(current);
            }
            return (pages, lines.len() - idx);
        }
        if current.chars().count() + cost > PAGE_LIMIT {
            if pages.len() + 1 >= MAX_PAGES {
                pages.push(current);
                return (pages, lines.len() - idx);
            }
            pages.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
        used += cost;
    }
    if !current.is_empty() {
        pages.push(current);
    }
    (pages, 0)
}

fn verification(level: VerificationLevel) -> &'static str {
    match level {
        VerificationLevel::None => "None",
        VerificationLevel::Low => "Low (verified email)",
        VerificationLevel::Medium => "Medium (on Discord 5+ minutes)",
        VerificationLevel::High => "High (in the server 10+ minutes)",
        VerificationLevel::Higher => "Highest (verified phone)",
        _ => "Unknown",
    }
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

    fn role(id: u64, position: i64, managed: bool) -> (RoleId, i64, bool) {
        (RoleId::new(id), position, managed)
    }

    /// Highest first, ties by id, bot roles marked, @everyone at the bottom.
    #[test]
    fn roles_list_from_the_top_of_the_hierarchy() {
        let everyone = RoleId::new(1);
        let lines = hierarchy(
            vec![role(1, 0, false), role(30, 2, false), role(20, 5, true), role(40, 2, false), role(10, 9, false)],
            everyone,
        );
        assert_eq!(
            lines,
            vec![
                "`1.` <@&10>".to_string(),
                "`2.` <@&20> · bot".to_string(),
                "`3.` <@&30>".to_string(),
                "`4.` <@&40>".to_string(),
                "`5.` @everyone".to_string(),
            ]
        );
    }

    /// Discord's maximum of 250 roles still sends: every page and the whole
    /// message stay within Discord's limits, and anything cut is the bottom of
    /// the list and counted.
    #[test]
    fn a_full_role_list_fits_discords_limits() {
        let rows: Vec<_> = (2..=251).map(|n| role(1_561_145_000_000_000_000 + n, n as i64, false)).collect();
        let lines = hierarchy(rows, RoleId::new(1));
        let (pages, dropped) = pack_pages(&lines);
        assert!(pages.len() <= MAX_PAGES);
        assert!(pages.iter().all(|p| p.chars().count() <= 4096));
        let shown: usize = pages.iter().map(|p| p.lines().count()).sum();
        assert_eq!(shown + dropped, lines.len(), "every role is shown or counted");
        assert!(pages.iter().map(|p| p.chars().count()).sum::<usize>() <= 6000 - 200);
        assert!(pages[0].starts_with("`1.`"), "the top of the hierarchy comes first");
    }

    /// A normal server's roles all fit, nothing cut.
    #[test]
    fn a_normal_role_list_is_shown_in_full() {
        let rows: Vec<_> = (2..=40).map(|n| role(1_561_145_000_000_000_000 + n, n as i64, false)).collect();
        let lines = hierarchy(rows, RoleId::new(1));
        let (pages, dropped) = pack_pages(&lines);
        assert_eq!(dropped, 0);
        assert_eq!(pages.len(), 1);
    }

    #[test]
    fn the_button_opens_the_roles_view() {
        let json = serde_json::to_string(&view_roles_row()).unwrap();
        assert!(json.contains(VIEW_ROLES_ID) && json.contains("View Roles"), "{json}");
    }
}
