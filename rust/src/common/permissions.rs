//! Owner / mod / whitelist checks and the moderation hierarchy guard.

use serenity::builder::CreateMessage;
use serenity::http::Http;
use serenity::model::guild::Member;
use serenity::model::id::{RoleId, UserId};

use super::config::{BOT_OWNER_IDS, DANGER_PERMS_MASK};
use super::guildinfo::GuildInfo;
use crate::state::guild_settings::gc;

pub fn is_owner(user_id: UserId) -> bool {
    is_owner_str(&user_id.to_string())
}

pub fn is_owner_str(user_id: &str) -> bool {
    BOT_OWNER_IDS.contains(user_id)
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

/// Holds the Administrator permission in this server.
///
/// A slash command's member record carries the caller's computed permissions,
/// so that is used when present. A fetched member has none, so fall back to
/// their roles plus @everyone from the cached guild.
pub fn is_guild_admin(member: &Member, info: &GuildInfo) -> bool {
    if let Some(p) = member.permissions {
        return p.administrator();
    }
    let everyone = RoleId::new(info.id.get());
    std::iter::once(&everyone)
        .chain(member.roles.iter())
        .any(|r| info.roles.get(r).is_some_and(|role| role.permissions.administrator()))
}

/// Why the bot shouldn't hand `role` out, if it shouldn't.
///
/// The bot outranks most roles, so whatever it assigns gets around Discord's
/// rule that you can only give out roles below your own. A role anti-nuke
/// trusts (whitelisted, or a failsafe target) would make whoever got it immune
/// to anti-nuke; one with a dangerous permission would make them powerful.
/// `allow_dangerous` is for roles that are meant to carry power, such as the
/// staff role an accepted application grants.
pub fn unsafe_grant_reason(info: &GuildInfo, role: RoleId, allow_dangerous: bool) -> Option<&'static str> {
    let g = gc(&info.id.to_string());
    let id = role.to_string();
    if g.nuke_whitelist_role_ids.contains(&id) {
        return Some("it's on the anti-nuke whitelist");
    }
    if g.failsafe_role_ids.contains(&id) {
        return Some("it's one of the failsafe roles");
    }
    if !allow_dangerous && info.roles.get(&role).is_some_and(|r| r.permissions.intersects(*DANGER_PERMS_MASK)) {
        return Some("it carries a dangerous permission (Administrator, Manage Server, Roles, Channels or Webhooks, Ban or Kick)");
    }
    None
}

/// Whether `role` sits at or above `member`'s highest role, i.e. one they
/// couldn't hand out themselves.
pub fn role_at_or_above(info: &GuildInfo, member: &Member, role: RoleId) -> bool {
    info.roles.get(&role).is_some_and(|r| r.position >= info.member_highest(member))
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
/// DM a ready-made embed. Silently does nothing when DMs are closed.
pub async fn try_dm_embed(http: &Http, user_id: UserId, embed: serenity::builder::CreateEmbed) {
    if let Ok(user) = user_id.to_user(http).await {
        let _ = user.direct_message(http, CreateMessage::new().embed(embed)).await;
    }
}

pub async fn try_dm(http: &Http, user_id: UserId, text: &str) {
    if let Ok(user) = user_id.to_user(http).await {
        let card = crate::common::theme::card(crate::common::theme::Tone::Warning, Some("📬 Message from Guardian"), text);
        let _ = user.direct_message(http, CreateMessage::new().embed(card)).await;
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
    // Bot owners get past the bot's own protections (the whitelist and the
    // role ladder). What's left below is Discord's limit, not the bot's.
    let actor_is_bot_owner = is_owner(actor.user.id);
    if !actor_is_bot_owner && is_whitelisted(target, info.owner_id) {
        return Err("That user's whitelisted, so they're protected.".into());
    }
    if target.user.id == actor.user.id {
        return Err("You can't do that to yourself.".into());
    }
    let target_pos = info.member_highest(target);
    if info.bot_highest > 0 && target_pos >= info.bot_highest {
        return Err("Their top role sits above mine, so I can't. Bump my role higher and try again.".into());
    }
    let actor_privileged = actor_is_bot_owner || actor.user.id == info.owner_id;
    if !actor_privileged && target_pos >= info.member_highest(actor) {
        return Err("Their role is the same as or higher than yours, so this one's out of your reach.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::guildinfo::RoleInfo;
    use serenity::model::id::{GuildId, RoleId};
    use std::collections::HashMap;

    fn member(guild: GuildId, id: u64, roles: &[u64]) -> Member {
        let mut m = Member::default();
        m.guild_id = guild;
        m.user.id = UserId::new(id);
        m.roles = roles.iter().map(|r| RoleId::new(*r)).collect();
        m
    }

    fn role(position: i64) -> RoleInfo {
        RoleInfo {
            name: String::new(),
            position,
            managed: false,
            permissions: serenity::model::Permissions::empty(),
            colour: 0,
            hoist: false,
            mentionable: false,
        }
    }

    fn admin_role() -> RoleInfo {
        RoleInfo { permissions: serenity::model::Permissions::ADMINISTRATOR, ..role(3) }
    }

    fn guild_with(roles: &[(u64, RoleInfo)]) -> GuildInfo {
        GuildInfo {
            id: GuildId::new(880_002),
            name: String::new(),
            owner_id: UserId::new(1),
            roles: roles.iter().map(|(id, r)| (RoleId::new(*id), r.clone())).collect(),
            bot_highest: 9,
        }
    }

    /// The permissions on a slash command's member record are authoritative.
    #[test]
    fn interaction_permissions_decide_admin() {
        let info = guild_with(&[]);
        let mut m = member(info.id, 500, &[]);
        m.permissions = Some(serenity::model::Permissions::ADMINISTRATOR | serenity::model::Permissions::SEND_MESSAGES);
        assert!(is_guild_admin(&m, &info));
        m.permissions = Some(serenity::model::Permissions::MANAGE_GUILD | serenity::model::Permissions::BAN_MEMBERS);
        assert!(!is_guild_admin(&m, &info), "Manage Server alone is not Administrator");
    }

    /// Without them (a fetched member), it comes from the member's roles,
    /// @everyone included.
    #[test]
    fn roles_decide_admin_when_the_member_has_no_permissions() {
        let info = guild_with(&[(10, role(1)), (20, admin_role())]);
        assert!(is_guild_admin(&member(info.id, 500, &[10, 20]), &info));
        assert!(!is_guild_admin(&member(info.id, 501, &[10]), &info));
        assert!(!is_guild_admin(&member(info.id, 502, &[]), &info));

        let everyone_admin = guild_with(&[(880_002, admin_role())]);
        assert!(is_guild_admin(&member(everyone_admin.id, 503, &[]), &everyone_admin));
    }

    /// The bot must not be made to hand out a role anti-nuke trusts, or one
    /// with real power, on someone else's say-so.
    #[test]
    fn trusted_and_powerful_roles_are_unsafe_to_grant() {
        let info = GuildInfo { id: GuildId::new(880_003), ..guild_with(&[(10, role(1)), (20, admin_role()), (30, role(2)), (40, role(2))]) };
        let gid = info.id.to_string();
        crate::state::guild_settings::update(&gid, |s| {
            s.nuke_whitelist_role_ids.push("30".into());
            s.failsafe_role_ids.push("40".into());
        });

        assert_eq!(unsafe_grant_reason(&info, RoleId::new(10), false), None, "a plain role is fine");
        assert!(unsafe_grant_reason(&info, RoleId::new(20), false).is_some(), "Administrator is dangerous");
        assert_eq!(unsafe_grant_reason(&info, RoleId::new(20), true), None, "unless power is the point");
        assert!(unsafe_grant_reason(&info, RoleId::new(30), true).is_some(), "whitelisted, whatever its permissions");
        assert!(unsafe_grant_reason(&info, RoleId::new(40), true).is_some(), "a failsafe target, likewise");
    }

    #[test]
    fn a_role_at_or_above_your_own_is_out_of_reach() {
        let info = guild_with(&[(10, role(1)), (20, role(3)), (30, role(5))]);
        let caller = member(info.id, 600, &[20]);
        assert!(!role_at_or_above(&info, &caller, RoleId::new(10)));
        assert!(role_at_or_above(&info, &caller, RoleId::new(20)), "your own top role");
        assert!(role_at_or_above(&info, &caller, RoleId::new(30)));
    }

    #[test]
    fn bot_owners_get_past_the_whitelist_and_the_role_ladder() {
        let guild = GuildId::new(880_001);
        let bot_owner: u64 = BOT_OWNER_IDS.iter().next().and_then(|s| s.parse().ok()).expect("a bot owner is configured");
        let info = GuildInfo {
            id: guild,
            name: String::new(),
            owner_id: UserId::new(1),
            roles: HashMap::from([(RoleId::new(10), role(2)), (RoleId::new(20), role(5))]),
            bot_highest: 9,
        };
        crate::state::guild_settings::update(&guild.to_string(), |s| s.nuke_whitelist_user_ids.push("300".into()));

        let staff = member(guild, 200, &[10]);
        let whitelisted = member(guild, 300, &[]);
        let senior = member(guild, 400, &[20]);
        let owner = member(guild, bot_owner, &[]);

        // Staff are held back by both.
        assert!(can_act_on(&info, &staff, &whitelisted).is_err());
        assert!(can_act_on(&info, &staff, &senior).is_err());
        // A bot owner with no roles at all is not.
        assert!(can_act_on(&info, &owner, &whitelisted).is_ok());
        assert!(can_act_on(&info, &owner, &senior).is_ok());
        // Discord's own limits still apply to everyone.
        assert!(can_act_on(&info, &owner, &member(guild, 1, &[])).is_err(), "the server owner can't be actioned");
        let above_bot = GuildInfo { bot_highest: 4, ..info };
        assert!(can_act_on(&above_bot, &owner, &senior).is_err(), "nor anyone above the bot");
    }
}
