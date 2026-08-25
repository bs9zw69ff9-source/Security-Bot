//! Slash command definitions.

use serenity::builder::{CreateCommand, CreateCommandOption};
use serenity::model::application::CommandOptionType;
use serenity::model::channel::ChannelType;

const TEXT_LIKE: [ChannelType; 2] = [ChannelType::Text, ChannelType::News];

fn opt(kind: CommandOptionType, name: &str, desc: &str) -> CreateCommandOption {
    CreateCommandOption::new(kind, name, desc)
}
fn req(kind: CommandOptionType, name: &str, desc: &str) -> CreateCommandOption {
    opt(kind, name, desc).required(true)
}
fn sub(name: &str, desc: &str) -> CreateCommandOption {
    opt(CommandOptionType::SubCommand, name, desc)
}
fn add_remove(name: &str, desc: &str) -> CreateCommandOption {
    req(CommandOptionType::String, name, desc)
        .add_string_choice("add", "add")
        .add_string_choice("remove", "remove")
}

pub fn all() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("mute")
            .description("Mute a member")
            .add_option(req(CommandOptionType::User, "user", "Member to mute"))
            .add_option(opt(CommandOptionType::Integer, "minutes", "Duration in minutes (0 = permanent)").min_int_value(0))
            .add_option(opt(CommandOptionType::String, "reason", "Reason for mute")),

        CreateCommand::new("unmute")
            .description("Unmute a member")
            .add_option(req(CommandOptionType::User, "user", "Member to unmute")),

        CreateCommand::new("kick")
            .description("Kick a member")
            .add_option(req(CommandOptionType::User, "user", "Member to kick"))
            .add_option(opt(CommandOptionType::String, "reason", "Reason for kick")),

        CreateCommand::new("ban")
            .description("Ban a member")
            .add_option(req(CommandOptionType::User, "user", "Member to ban"))
            .add_option(opt(CommandOptionType::String, "reason", "Reason for ban"))
            .add_option(
                opt(CommandOptionType::Integer, "delete_days", "Days of messages to delete (0–7)")
                    .min_int_value(0)
                    .max_int_value(7),
            ),

        CreateCommand::new("unban")
            .description("Unban a user by ID")
            .add_option(req(CommandOptionType::String, "user_id", "The user ID to unban"))
            .add_option(opt(CommandOptionType::String, "reason", "Reason for unban")),

        CreateCommand::new("purge")
            .description("Bulk-delete messages in this channel")
            .add_option(
                req(CommandOptionType::Integer, "count", "Number of messages (1–100)").min_int_value(1).max_int_value(100),
            )
            .add_option(opt(CommandOptionType::User, "user", "Only delete messages from this user (optional)")),

        CreateCommand::new("lockdown")
            .description("Lock or unlock a channel")
            .add_option(
                req(CommandOptionType::String, "action", "Lock or unlock")
                    .add_string_choice("lock", "lock")
                    .add_string_choice("unlock", "unlock"),
            )
            .add_option(opt(CommandOptionType::Channel, "channel", "Channel to lock/unlock (defaults to current)")),

        CreateCommand::new("panic").description("EMERGENCY: lock every text channel at once (owner only)"),

        CreateCommand::new("warn")
            .description("Issue a warning to a member")
            .add_option(req(CommandOptionType::User, "user", "Member to warn"))
            .add_option(opt(CommandOptionType::String, "reason", "Reason for warning")),

        CreateCommand::new("warnings")
            .description("View a member's warnings")
            .add_option(req(CommandOptionType::User, "user", "Member to inspect")),

        CreateCommand::new("clearwarns")
            .description("Clear all warnings for a member")
            .add_option(req(CommandOptionType::User, "user", "Member to clear")),

        CreateCommand::new("config").description("View Guardian configuration (bot owner only)"),
        CreateCommand::new("nuketest").description("Confirm anti-nuke system is active (owner only)"),
        CreateCommand::new("status").description("Bot health: uptime, latency, guild count, memory (bot owner only)"),
        CreateCommand::new("limits").description("Check your remaining mod action limits for today"),

        CreateCommand::new("antiping")
            .description("Configure anti-ping protection for staff/VIPs")
            .add_option(sub("status", "Show current anti-ping settings"))
            .add_option(sub("toggle", "Enable or disable anti-ping")
                .add_sub_option(req(CommandOptionType::Boolean, "enabled", "On or off")))
            .add_option(sub("action", "Set punishment for pinging a protected target")
                .add_sub_option(
                    req(CommandOptionType::String, "type", "Punishment")
                        .add_string_choice("none (log only)", "none")
                        .add_string_choice("warn", "warn")
                        .add_string_choice("mute (mute role)", "mute")
                        .add_string_choice("timeout (native)", "timeout"),
                ))
            .add_option(sub("duration", "Mute/timeout duration in minutes")
                .add_sub_option(
                    req(CommandOptionType::Integer, "minutes", "Minutes").min_int_value(1).max_int_value(40320),
                ))
            .add_option(sub("delete", "Delete the offending message?")
                .add_sub_option(req(CommandOptionType::Boolean, "enabled", "True to delete")))
            .add_option(sub("ignorereplies", "Ignore reply-pings?")
                .add_sub_option(req(CommandOptionType::Boolean, "enabled", "True to ignore reply pings")))
            .add_option(sub("response", "Customize the warning message - {user} {targets} {action}")
                .add_sub_option(req(CommandOptionType::String, "text", "Template text, or 'default' to reset")))
            .add_option(sub("notify", "Post the public warning message in the channel?")
                .add_sub_option(req(CommandOptionType::Boolean, "enabled", "True to post warning in channel")))
            .add_option(sub("protect", "Add/remove a protected user")
                .add_sub_option(add_remove("action", "add or remove"))
                .add_sub_option(req(CommandOptionType::User, "user", "User to protect")))
            .add_option(sub("protectrole", "Add/remove a protected role")
                .add_sub_option(add_remove("action", "add or remove"))
                .add_sub_option(req(CommandOptionType::Role, "role", "Role to protect")))
            .add_option(sub("list", "List protected users and roles")),

        CreateCommand::new("setup")
            .description("Configure Guardian for this server")
            .add_option(sub("quick", "Auto-provision a Muted role + log/alert/message-log channels in one step")
                .add_sub_option(opt(CommandOptionType::Role, "mod_role", "Role allowed to use moderation commands (optional)")))
            .add_option(sub("view", "Show current configuration for this server"))
            .add_option(sub("roles", "Set the mod role and/or mute role")
                .add_sub_option(opt(CommandOptionType::Role, "mod_role", "Role allowed to use moderation commands"))
                .add_sub_option(opt(CommandOptionType::Role, "mute_role", "Role applied on mute (must deny Send Messages)")))
            .add_option(sub("channels", "Set log/alert/message-log channels")
                .add_sub_option(opt(CommandOptionType::Channel, "log_channel", "Security log channel").channel_types(TEXT_LIKE.into()))
                .add_sub_option(opt(CommandOptionType::Channel, "alert_channel", "Critical-alert channel (owner pinged)").channel_types(TEXT_LIKE.into()))
                .add_sub_option(opt(CommandOptionType::Channel, "msg_log_channel", "Deleted / edited message log channel").channel_types(TEXT_LIKE.into())))
            .add_option(sub("whitelist", "Add/remove an anti-nuke whitelist entry")
                .add_sub_option(add_remove("action", "add or remove"))
                .add_sub_option(opt(CommandOptionType::User, "user", "User to whitelist"))
                .add_sub_option(opt(CommandOptionType::Role, "role", "Role to whitelist")))
            .add_option(sub("failsafe", "Add/remove a role targeted by !failsafe")
                .add_sub_option(add_remove("action", "add or remove"))
                .add_sub_option(req(CommandOptionType::Role, "role", "Role to add/remove from the failsafe target list"))),

        CreateCommand::new("tickets")
            .description("Configure the ticket system")
            .add_option(sub("addtype", "Add or update a ticket type")
                .add_sub_option(req(CommandOptionType::String, "key", "Short internal id, e.g. report_player"))
                .add_sub_option(req(CommandOptionType::String, "label", "Button label shown to users"))
                .add_sub_option(req(CommandOptionType::String, "emoji", "Emoji for the button (e.g. 🚨)"))
                .add_sub_option(req(CommandOptionType::Channel, "log_channel", "Where this type's logs + transcripts go")
                    .channel_types(vec![ChannelType::Text])))
            .add_option(sub("removetype", "Remove a ticket type")
                .add_sub_option(req(CommandOptionType::String, "key", "The type's key")))
            .add_option(sub("listtypes", "List configured ticket types"))
            .add_option(sub("category", "Set the category new ticket channels are created under")
                .add_sub_option(req(CommandOptionType::Channel, "category", "Category channel")
                    .channel_types(vec![ChannelType::Category])))
            .add_option(sub("panel", "Post or refresh the ticket panel")
                .add_sub_option(opt(CommandOptionType::Channel, "channel", "Channel to post in (defaults to the last-used one)")
                    .channel_types(TEXT_LIKE.into()))),

        CreateCommand::new("applications")
            .description("Configure the application system")
            .add_option(sub("list", "List configured applications and their channels/roles"))
            .add_option(sub("panel", "Post or refresh an application's panel (Apply button)")
                .add_sub_option(req(CommandOptionType::String, "key", "The application's key, e.g. gambino"))
                .add_sub_option(opt(CommandOptionType::Channel, "channel", "Channel to post in (defaults to its configured one)")
                    .channel_types(TEXT_LIKE.into())))
            .add_option(sub("setreview", "Set where submitted applications go for staff review")
                .add_sub_option(req(CommandOptionType::String, "key", "The application's key"))
                .add_sub_option(req(CommandOptionType::Channel, "channel", "Review channel").channel_types(vec![ChannelType::Text])))
            .add_option(sub("setpanelchannel", "Set where an application's panel posts, or use key:all for one shared panel")
                .add_sub_option(req(CommandOptionType::String, "key", "The application's key, or 'all' to put them on one panel"))
                .add_sub_option(req(CommandOptionType::Channel, "channel", "Panel channel").channel_types(TEXT_LIKE.into())))
            .add_option(sub("addrole", "Add a role granted when an application is accepted")
                .add_sub_option(req(CommandOptionType::String, "key", "The application's key"))
                .add_sub_option(req(CommandOptionType::Role, "role", "Role to grant on accept")))
            .add_option(sub("removerole", "Remove an accepted-role from an application")
                .add_sub_option(req(CommandOptionType::String, "key", "The application's key"))
                .add_sub_option(req(CommandOptionType::Role, "role", "Role to remove")))
            .add_option(sub("setquestions", "Replace an application's questions")
                .add_sub_option(req(CommandOptionType::String, "key", "The application's key"))
                .add_sub_option(req(CommandOptionType::String, "questions", "Questions separated by | (pipe), in order").max_length(4000)))
            .add_option(sub("open", "Open an application so users can apply (or 'all')")
                .add_sub_option(req(CommandOptionType::String, "key", "The application's key, or 'all' for every application")))
            .add_option(sub("close", "Close an application so users can't apply (or 'all')")
                .add_sub_option(req(CommandOptionType::String, "key", "The application's key, or 'all' for every application"))),

        CreateCommand::new("police")
            .description("Police department resources")
            .add_option(
                opt(CommandOptionType::SubCommandGroup, "manual", "Officer guide & procedures manual").add_sub_option(
                    sub("setup", "Post the officer guide & procedures manual in a channel").add_sub_option(
                        opt(CommandOptionType::Channel, "channel", "Channel to post in (defaults to this channel)")
                            .channel_types(TEXT_LIKE.into()),
                    ),
                ),
            ),

        CreateCommand::new("chainofcommand")
            .description("Auto-updating chain of command")
            .add_option(sub("setup", "Post (or move) a chain-of-command board")
                .add_sub_option(opt(CommandOptionType::String, "key", "Board id, e.g. 'police' (defaults to the main 'default' board)"))
                .add_sub_option(opt(CommandOptionType::Channel, "channel", "Channel to post in (defaults to this channel)")
                    .channel_types(TEXT_LIKE.into()))
                .add_sub_option(opt(CommandOptionType::String, "title", "Embed title (defaults to '📋 Chain of Command')")))
            .add_option(sub("setroles", "Replace a board's whole role list with one flat, unlabeled group")
                .add_sub_option(req(CommandOptionType::String, "roles", "Roles in order, mentioned or as IDs, separated by spaces or commas"))
                .add_sub_option(opt(CommandOptionType::String, "key", "Board id (defaults to 'default')")))
            .add_option(sub("setgroup", "Add or replace one labeled group within a board")
                .add_sub_option(req(CommandOptionType::String, "label", "Group header, e.g. 'Ranks'"))
                .add_sub_option(req(CommandOptionType::String, "roles", "Roles in order, mentioned or as IDs, separated by spaces or commas"))
                .add_sub_option(opt(CommandOptionType::String, "key", "Board id (defaults to 'default')")))
            .add_option(sub("removegroup", "Remove one labeled group from a board")
                .add_sub_option(req(CommandOptionType::String, "label", "Group header to remove"))
                .add_sub_option(opt(CommandOptionType::String, "key", "Board id (defaults to 'default')")))
            .add_option(sub("refresh", "Manually re-render a board now")
                .add_sub_option(opt(CommandOptionType::String, "key", "Board id (defaults to 'default')")))
            .add_option(sub("view", "Show a board's configured channel and groups")
                .add_sub_option(opt(CommandOptionType::String, "key", "Board id (defaults to 'default')")))
            .add_option(sub("list", "List every board configured for this server")),

        CreateCommand::new("help").description("Show all Guardian Bot commands"),
    ]
}
