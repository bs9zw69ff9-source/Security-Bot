//! Owner / mod / whitelist checks and the moderation hierarchy guard.

use serenity::builder::CreateMessage;
use serenity::http::Http;
use serenity::model::guild::Member;
use serenity::model::id::UserId;

use super::config::BOT_OWNER_IDS;
use super::guildinfo::GuildInfo;
use crate::state::guild_settings::gc;

pub fn is_owner(user_id: UserId) -> bool {
    BOT_OWNER_IDS.contains(&user_id.to_string())
}

pub fn is_mod(member: &Member, guild_owner_id: UserId) -> bool {
    if is_owner(member.user.id) {
        return true;
    }
    if member.user.id == guild_owner_id {
        return true;
    }
    let mod_role_id = gc(&member.guild_id.to_string()).mod_role_id;
    if mod_role_id.is_empty() {
        return false;
    }
    member.roles.iter().any(|r| r.to_string() == mod_role_id)
}

pub fn is_whitelisted(member: &Member, guild_owner_id: UserId) -> bool {
    if is_owner(member.user.id) {
        return true; // hardcoded owner is always immune
    }
    if member.user.id == guild_owner_id {
        return true;
    }
    let g = gc(&member.guild_id.to_string());
    if g.nuke_whitelist_user_ids.contains(&member.user.id.to_string()) {
        return true;
    }
    member.roles.iter().any(|r| g.nuke_whitelist_role_ids.contains(&r.to_string()))
}

/// Best-effort DM to a member before punitive action.
pub async fn try_dm(http: &Http, user_id: UserId, text: &str) {
    if let Ok(user) = user_id.to_user(http).await {
        let _ = user.direct_message(http, CreateMessage::new().content(text)).await;
    }
}

/// Guard: can `actor` moderate `target`? Protects owner/whitelist and respects
/// hierarchy. `Ok(())` means go ahead; `Err(why)` is the user-facing reason.
pub fn can_act_on(info: &GuildInfo, actor: &Member, target: &Member) -> Result<(), String> {
    if is_owner(target.user.id) {
        return Err("That's the bot owner, so they're off-limits.".into());
    }
    if target.user.id == info.owner_id {
        return Err("That's the server owner - can't touch them.".into());
    }
    if is_whitelisted(target, info.owner_id) {
        return Err("That user's whitelisted, so they're protected.".into());
    }
    if target.user.id == actor.user.id {
        return Err("You can't do that to yourself.".into());
    }
    let target_pos = info.member_highest(target);
    if info.bot_highest > 0 && target_pos >= info.bot_highest {
        return Err("Their top role sits above mine, so I can't. Bump my role higher and try again.".into());
    }
    let actor_privileged = is_owner(actor.user.id) || actor.user.id == info.owner_id;
    if !actor_privileged && target_pos >= info.member_highest(actor) {
        return Err("Their role is the same as or higher than yours, so this one's out of your reach.".into());
    }
    Ok(())
}
