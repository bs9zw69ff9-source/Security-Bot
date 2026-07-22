// ── Slash Commands ────────────────────────────────────────────
const { SlashCommandBuilder, ChannelType } = require("discord.js");

const commands = [
  new SlashCommandBuilder()
    .setName("mute").setDescription("Mute a member")
    .addUserOption(o => o.setName("user").setDescription("Member to mute").setRequired(true))
    .addIntegerOption(o => o.setName("minutes").setDescription("Duration in minutes (0 = permanent)").setMinValue(0))
    .addStringOption(o => o.setName("reason").setDescription("Reason for mute")),

  new SlashCommandBuilder()
    .setName("unmute").setDescription("Unmute a member")
    .addUserOption(o => o.setName("user").setDescription("Member to unmute").setRequired(true)),

  new SlashCommandBuilder()
    .setName("kick").setDescription("Kick a member")
    .addUserOption(o => o.setName("user").setDescription("Member to kick").setRequired(true))
    .addStringOption(o => o.setName("reason").setDescription("Reason for kick")),

  new SlashCommandBuilder()
    .setName("ban").setDescription("Ban a member")
    .addUserOption(o => o.setName("user").setDescription("Member to ban").setRequired(true))
    .addStringOption(o => o.setName("reason").setDescription("Reason for ban"))
    .addIntegerOption(o => o.setName("delete_days").setDescription("Days of messages to delete (0–7)").setMinValue(0).setMaxValue(7)),

  new SlashCommandBuilder()
    .setName("unban").setDescription("Unban a user by ID")
    .addStringOption(o => o.setName("user_id").setDescription("The user ID to unban").setRequired(true))
    .addStringOption(o => o.setName("reason").setDescription("Reason for unban")),

  new SlashCommandBuilder()
    .setName("purge").setDescription("Bulk-delete messages in this channel")
    .addIntegerOption(o => o.setName("count").setDescription("Number of messages (1–100)").setRequired(true).setMinValue(1).setMaxValue(100))
    .addUserOption(o => o.setName("user").setDescription("Only delete messages from this user (optional)")),

  new SlashCommandBuilder()
    .setName("lockdown").setDescription("Lock or unlock a channel")
    .addStringOption(o =>
      o.setName("action").setDescription("Lock or unlock").setRequired(true)
        .addChoices({ name: "lock", value: "lock" }, { name: "unlock", value: "unlock" }))
    .addChannelOption(o => o.setName("channel").setDescription("Channel to lock/unlock (defaults to current)")),

  new SlashCommandBuilder()
    .setName("panic").setDescription("EMERGENCY: lock every text channel at once (owner only)"),

  new SlashCommandBuilder()
    .setName("warn").setDescription("Issue a warning to a member")
    .addUserOption(o => o.setName("user").setDescription("Member to warn").setRequired(true))
    .addStringOption(o => o.setName("reason").setDescription("Reason for warning")),

  new SlashCommandBuilder()
    .setName("warnings").setDescription("View a member's warnings")
    .addUserOption(o => o.setName("user").setDescription("Member to inspect").setRequired(true)),

  new SlashCommandBuilder()
    .setName("clearwarns").setDescription("Clear all warnings for a member")
    .addUserOption(o => o.setName("user").setDescription("Member to clear").setRequired(true)),

  new SlashCommandBuilder()
    .setName("config").setDescription("View Guardian configuration (bot owner only)"),

  new SlashCommandBuilder()
    .setName("nuketest").setDescription("Confirm anti-nuke system is active (owner only)"),

  new SlashCommandBuilder()
    .setName("status").setDescription("Bot health: uptime, latency, guild count, memory (bot owner only)"),

  new SlashCommandBuilder()
    .setName("limits").setDescription("Check your remaining mod action limits for today"),

  // ── Anti-Ping (customizable) ──
  new SlashCommandBuilder()
    .setName("antiping").setDescription("Configure anti-ping protection for staff/VIPs")
    .addSubcommand(s => s.setName("status").setDescription("Show current anti-ping settings"))
    .addSubcommand(s => s.setName("toggle").setDescription("Enable or disable anti-ping")
      .addBooleanOption(o => o.setName("enabled").setDescription("On or off").setRequired(true)))
    .addSubcommand(s => s.setName("action").setDescription("Set punishment for pinging a protected target")
      .addStringOption(o => o.setName("type").setDescription("Punishment").setRequired(true)
        .addChoices(
          { name: "none (log only)",   value: "none"    },
          { name: "warn",              value: "warn"    },
          { name: "mute (mute role)",  value: "mute"    },
          { name: "timeout (native)",  value: "timeout" },
        )))
    .addSubcommand(s => s.setName("duration").setDescription("Mute/timeout duration in minutes")
      .addIntegerOption(o => o.setName("minutes").setDescription("Minutes").setRequired(true).setMinValue(1).setMaxValue(40320)))
    .addSubcommand(s => s.setName("delete").setDescription("Delete the offending message?")
      .addBooleanOption(o => o.setName("enabled").setDescription("True to delete").setRequired(true)))
    .addSubcommand(s => s.setName("ignorereplies").setDescription("Ignore reply-pings?")
      .addBooleanOption(o => o.setName("enabled").setDescription("True to ignore reply pings").setRequired(true)))
    .addSubcommand(s => s.setName("response").setDescription("Customize the warning message - {user} {targets} {action}")
      .addStringOption(o => o.setName("text").setDescription("Template text, or 'default' to reset").setRequired(true)))
    .addSubcommand(s => s.setName("notify").setDescription("Post the public warning message in the channel?")
      .addBooleanOption(o => o.setName("enabled").setDescription("True to post warning in channel").setRequired(true)))
    .addSubcommand(s => s.setName("protect").setDescription("Add/remove a protected user")
      .addStringOption(o => o.setName("action").setDescription("add or remove").setRequired(true)
        .addChoices({ name: "add", value: "add" }, { name: "remove", value: "remove" }))
      .addUserOption(o => o.setName("user").setDescription("User to protect").setRequired(true)))
    .addSubcommand(s => s.setName("protectrole").setDescription("Add/remove a protected role")
      .addStringOption(o => o.setName("action").setDescription("add or remove").setRequired(true)
        .addChoices({ name: "add", value: "add" }, { name: "remove", value: "remove" }))
      .addRoleOption(o => o.setName("role").setDescription("Role to protect").setRequired(true)))
    .addSubcommand(s => s.setName("list").setDescription("List protected users and roles")),

  new SlashCommandBuilder()
    .setName("setup").setDescription("Configure Guardian for this server")
    .addSubcommand(s => s.setName("quick").setDescription("Auto-provision a Muted role + log/alert/message-log channels in one step")
      .addRoleOption(o => o.setName("mod_role").setDescription("Role allowed to use moderation commands (optional)")))
    .addSubcommand(s => s.setName("view").setDescription("Show current configuration for this server"))
    .addSubcommand(s => s.setName("roles").setDescription("Set the mod role and/or mute role")
      .addRoleOption(o => o.setName("mod_role").setDescription("Role allowed to use moderation commands"))
      .addRoleOption(o => o.setName("mute_role").setDescription("Role applied on mute (must deny Send Messages)")))
    .addSubcommand(s => s.setName("channels").setDescription("Set log/alert/message-log channels")
      .addChannelOption(o => o.setName("log_channel").setDescription("Security log channel").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
      .addChannelOption(o => o.setName("alert_channel").setDescription("Critical-alert channel (owner pinged)").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
      .addChannelOption(o => o.setName("msg_log_channel").setDescription("Deleted / edited message log channel").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)))
    .addSubcommand(s => s.setName("whitelist").setDescription("Add/remove an anti-nuke whitelist entry")
      .addStringOption(o => o.setName("action").setDescription("add or remove").setRequired(true)
        .addChoices({ name: "add", value: "add" }, { name: "remove", value: "remove" }))
      .addUserOption(o => o.setName("user").setDescription("User to whitelist"))
      .addRoleOption(o => o.setName("role").setDescription("Role to whitelist")))
    .addSubcommand(s => s.setName("failsafe").setDescription("Add/remove a role targeted by !failsafe")
      .addStringOption(o => o.setName("action").setDescription("add or remove").setRequired(true)
        .addChoices({ name: "add", value: "add" }, { name: "remove", value: "remove" }))
      .addRoleOption(o => o.setName("role").setDescription("Role to add/remove from the failsafe target list").setRequired(true))),

  new SlashCommandBuilder()
    .setName("tickets").setDescription("Configure the ticket system")
    .addSubcommand(s => s.setName("addtype").setDescription("Add or update a ticket type")
      .addStringOption(o => o.setName("key").setDescription("Short internal id, e.g. report_player").setRequired(true))
      .addStringOption(o => o.setName("label").setDescription("Button label shown to users").setRequired(true))
      .addStringOption(o => o.setName("emoji").setDescription("Emoji for the button (e.g. 🚨)").setRequired(true))
      .addChannelOption(o => o.setName("log_channel").setDescription("Where this type's logs + transcripts go").setRequired(true).addChannelTypes(ChannelType.GuildText)))
    .addSubcommand(s => s.setName("removetype").setDescription("Remove a ticket type")
      .addStringOption(o => o.setName("key").setDescription("The type's key").setRequired(true)))
    .addSubcommand(s => s.setName("listtypes").setDescription("List configured ticket types"))
    .addSubcommand(s => s.setName("category").setDescription("Set the category new ticket channels are created under")
      .addChannelOption(o => o.setName("category").setDescription("Category channel").setRequired(true).addChannelTypes(ChannelType.GuildCategory)))
    .addSubcommand(s => s.setName("panel").setDescription("Post or refresh the ticket panel")
      .addChannelOption(o => o.setName("channel").setDescription("Channel to post in (defaults to the last-used one)").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))),

  new SlashCommandBuilder()
    .setName("applications").setDescription("Configure the application system")
    .addSubcommand(s => s.setName("list").setDescription("List configured applications and their channels/roles"))
    .addSubcommand(s => s.setName("panel").setDescription("Post or refresh an application's panel (Apply button)")
      .addStringOption(o => o.setName("key").setDescription("The application's key, e.g. gambino").setRequired(true))
      .addChannelOption(o => o.setName("channel").setDescription("Channel to post in (defaults to its configured one)").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)))
    .addSubcommand(s => s.setName("setreview").setDescription("Set where submitted applications go for staff review")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addChannelOption(o => o.setName("channel").setDescription("Review channel").setRequired(true).addChannelTypes(ChannelType.GuildText)))
    .addSubcommand(s => s.setName("setpanelchannel").setDescription("Set which channel an application's Apply panel posts to")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addChannelOption(o => o.setName("channel").setDescription("Panel channel").setRequired(true).addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)))
    .addSubcommand(s => s.setName("addrole").setDescription("Add a role granted when an application is accepted")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addRoleOption(o => o.setName("role").setDescription("Role to grant on accept").setRequired(true)))
    .addSubcommand(s => s.setName("removerole").setDescription("Remove an accepted-role from an application")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addRoleOption(o => o.setName("role").setDescription("Role to remove").setRequired(true)))
    .addSubcommand(s => s.setName("setquestions").setDescription("Replace an application's questions")
      .addStringOption(o => o.setName("key").setDescription("The application's key").setRequired(true))
      .addStringOption(o => o.setName("questions").setDescription("Questions separated by | (pipe), in order").setRequired(true).setMaxLength(4000)))
    .addSubcommand(s => s.setName("open").setDescription("Open an application so users can apply (or 'all')")
      .addStringOption(o => o.setName("key").setDescription("The application's key, or 'all' for every application").setRequired(true)))
    .addSubcommand(s => s.setName("close").setDescription("Close an application so users can't apply (or 'all')")
      .addStringOption(o => o.setName("key").setDescription("The application's key, or 'all' for every application").setRequired(true))),

  new SlashCommandBuilder()
    .setName("police").setDescription("Police department resources")
    .addSubcommandGroup(g => g.setName("manual").setDescription("Officer guide & procedures manual")
      .addSubcommand(s => s.setName("setup").setDescription("Post the officer guide & procedures manual in a channel")
        .addChannelOption(o => o.setName("channel").setDescription("Channel to post in (defaults to this channel)").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)))),

  new SlashCommandBuilder()
    .setName("chainofcommand").setDescription("Auto-updating chain of command")
    .addSubcommand(s => s.setName("setup").setDescription("Post (or move) a chain-of-command board")
      .addStringOption(o => o.setName("key").setDescription("Board id, e.g. 'police' (defaults to the main 'default' board)"))
      .addChannelOption(o => o.setName("channel").setDescription("Channel to post in (defaults to this channel)").addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
      .addStringOption(o => o.setName("title").setDescription("Embed title (defaults to '📋 Chain of Command')")))
    .addSubcommand(s => s.setName("setroles").setDescription("Replace a board's whole role list with one flat, unlabeled group")
      .addStringOption(o => o.setName("roles").setDescription("Roles in order, mentioned or as IDs, separated by spaces or commas").setRequired(true))
      .addStringOption(o => o.setName("key").setDescription("Board id (defaults to 'default')")))
    .addSubcommand(s => s.setName("setgroup").setDescription("Add or replace one labeled group within a board")
      .addStringOption(o => o.setName("label").setDescription("Group header, e.g. 'Ranks'").setRequired(true))
      .addStringOption(o => o.setName("roles").setDescription("Roles in order, mentioned or as IDs, separated by spaces or commas").setRequired(true))
      .addStringOption(o => o.setName("key").setDescription("Board id (defaults to 'default')")))
    .addSubcommand(s => s.setName("removegroup").setDescription("Remove one labeled group from a board")
      .addStringOption(o => o.setName("label").setDescription("Group header to remove").setRequired(true))
      .addStringOption(o => o.setName("key").setDescription("Board id (defaults to 'default')")))
    .addSubcommand(s => s.setName("refresh").setDescription("Manually re-render a board now")
      .addStringOption(o => o.setName("key").setDescription("Board id (defaults to 'default')")))
    .addSubcommand(s => s.setName("view").setDescription("Show a board's configured channel and groups")
      .addStringOption(o => o.setName("key").setDescription("Board id (defaults to 'default')")))
    .addSubcommand(s => s.setName("list").setDescription("List every board configured for this server")),

  new SlashCommandBuilder().setName("help").setDescription("Show all Guardian Bot commands"),
];

const commandBody = () => commands.map(c => c.toJSON());

module.exports = { commands, commandBody };
