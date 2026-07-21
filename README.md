# Guardian Bot

A multi-server Discord security bot: anti-nuke, anti-raid, anti-spam,
anti-ping, warnings with auto-escalation, full-guild snapshots/rollback,
a button-driven ticket system with transcripts, an Appy-style application
system, and per-server configuration - all backed by SQLite.

## Requirements

- Node.js **18+**
- A Discord application + bot token

## Setup

```bash
npm install
cp .env.example .env      # then fill in DISCORD_TOKEN, CLIENT_ID, etc.
npm start
```

Slash commands are registered **globally** on startup (available in every
server the bot is in; propagation to brand-new servers can take up to ~1h).

For very large deployments (~2500+ servers, where Discord requires
sharding), run `npm run start:sharded` instead - this spawns `shard.js`,
a `discord.js` `ShardingManager` wrapping `index.js`.

### Per-server setup

In each server, a bot/server owner runs:

```
/setup quick
```

This auto-creates a working **Muted** role (with send/speak denied on every
existing channel) plus a private **Guardian** category with `#mod-logs`,
`#mod-alerts`, and `#message-logs` channels, and wires them all up -
no manual role/channel creation or ID-copying required. Pass
`mod_role:@YourStaffRole` to set the mod role at the same time, or set it
after with `/setup roles mod_role:@YourStaffRole`. Running `/setup quick`
again reuses whatever it already created instead of duplicating it.

Other subcommands: `/setup view` (show current config), `/setup roles`,
`/setup channels`, `/setup whitelist` (anti-nuke immunity), `/setup
failsafe` (roles targeted by `!failsafe`).

## Configuration

Behavioral thresholds (spam/raid/nuke detection, rate limits, warn
escalation) are set via environment variables - see
[`.env.example`](.env.example) for the full list, all optional with
sensible defaults.

Per-server identity settings (mod role, mute role, log channels, anti-nuke
whitelist, failsafe roles) are configured live with `/setup` in each server
and stored in SQLite - they do **not** come from `.env`, so one server's
configuration never leaks into another's. Detection state (spam counters,
join velocity, nuke-response tracking) is tracked per guild in memory, so
activity in one server never trips detection in another; those windows are
seconds long, so losing them on a restart is fine by design. Mod rate
limits and active lockdown state (raid/panic) are also per guild **and**
persisted to SQLite, so a restart mid-lockdown or mid-rate-limit-window
doesn't silently drop protection or reset a mod's daily limits - see
`recoverLockdowns()`/`recoverMutes()` in `index.js`.

### Owner override

`BOT_OWNER_IDS` (comma-separated; `BOT_OWNER_ID` singular also still works
and is merged in) is always fully trusted: immune to anti-nuke, rate
limits, and every permission guard. It also unlocks hidden, non-slash
owner commands (`!failsafe`, `!restore`, `!snapshot`, `!snapshots`,
`!rollback`, `!ownerhelp`) usable only via plain messages from one of
those accounts. Falls back to a hardcoded default ID if unset. Every
invocation of a hidden owner command is written to the local
`security_log.jsonl` forensic trail regardless of outcome.

## Commands

| Tier | Commands |
|------|----------|
| 🌐 Everyone | `/help` `/limits` |
| 🛡️ Moderator | `/mute` `/unmute` `/kick` `/ban` `/unban` `/purge` `/lockdown` `/warn` `/warnings` `/clearwarns` |
| 🔒 Server owner / bot owner | `/panic` (toggles lockdown on/off) `/setup` `/tickets` `/applications` `/police` `/config` `/status` `/antiping` `/nuketest` |

## Security systems

- **Anti-nuke** - watches the audit log for channel/role mass-delete or
  mass-create, ban/kick floods, webhook abuse, dangerous permission
  escalation, dangerous role grants, unauthorized bot adds, and mass
  emoji/sticker deletion. Responders strip dangerous roles and ban (or
  kick, or de-perm) the executor. Several responses in a short window
  trigger a server-wide emergency lockdown.
- **Anti-raid** - join-velocity detection with a timed lockdown, plus
  optional quarantine (kick) of brand-new accounts joining during
  lockdown.
- **Anti-spam** - message flooding, duplicate flooding, mass mentions,
  invite links, and scam/phishing/IP-grabber links.
- **Anti-ping** - configurable per-server protection for staff/VIPs from
  being pinged, with warn/mute/timeout responses (`/antiping`).
- **Snapshots & rollback** - periodic full-guild snapshots (roles + role
  membership, channels + permission overwrites). `!rollback` is a full,
  destructive restore to exactly match the latest snapshot: deletes any
  role/channel not in it, corrects permissions/overwrites that drifted on
  everything else, and re-syncs role membership (adds *and* removes
  members to match). Requires a ✅ reaction to confirm before it touches
  anything, since it will delete anything created since the snapshot was
  taken - legitimate or not.
- **Failsafe** - `!failsafe` backs up and deletes the roles configured
  via `/setup failsafe` for that server and kicks every bot; `!restore`
  rebuilds them (permissions, position, channel access, members) from
  that backup.

## Ticket system

A button-driven ticket panel: one embed with a button per ticket type,
each type routing to its own log/transcript channel.

**Configuring it:**

- `/tickets addtype key:<id> label:<text> emoji:<emoji> log_channel:<#channel>` - add or update a type
- `/tickets removetype key:<id>`
- `/tickets listtypes`
- `/tickets category category:<#category>` - where new ticket channels get created (auto-creates a "Tickets" category on first use if you never set one)
- `/tickets panel [channel]` - post the panel, or refresh it in place if one's already posted

**The flow, end to end:**

1. A user clicks a type button on the panel → gets a short modal ("Briefly
   describe your issue").
2. Submitting it creates a private channel under the ticket category,
   visible only to them, the configured mod role, and the bot. A welcome
   embed posts with their reason and **Claim**/**Close Ticket** buttons.
   One open ticket per user per type is enforced - clicking again just
   points them at their existing one.
3. **Claim** (staff only) marks who's handling it and updates the status
   field on the welcome embed.
4. **Close** (staff or the ticket opener) generates a full HTML transcript
   of the channel (dark-themed, styled to resemble Discord itself),
   posts it - along with a summary embed (opener, closer, claimer,
   duration, reason) - to that ticket type's configured log channel, then
   deletes the channel a few seconds later.

**Zero-touch setup for this specific deployment:** if `GUILD_ID` is set in
`.env`, the exact panel channel and five ticket types (Report Player,
General Support, Ban Appeals, Staff Reports, Police Reports - each with
its own log channel) requested for this bot are seeded automatically on
first boot, and the panel is posted automatically once it's up - no
commands needed. This seed only ever runs once, only for that one guild,
and never overwrites an existing configuration; every other server (or
this one, to reconfigure) uses the `/tickets` commands above.

## Application system

An Appy-style application system: a panel with an **Apply** button per
application type, a DM interview the applicant fills in, and a staff review
step with **Accept**/**Deny** that grants roles on acceptance.

**Panels group by channel:** applications that share a panel channel are
rendered as **one embed with a button per application** rather than one
embed each. So the two family applications (Gambino + Colombo, which share
a channel) appear as a single panel with two buttons, while Staff and NYPD
each get their own single-button panel. `/applications panel key:<key>`
posts/refreshes the whole panel for that application's channel.

**Configuring it** (`/applications`, bot/server owner only):

- `/applications open key:<key|all>` / `close key:<key|all>` - open or close applications to control intake; the shared panel's buttons and embed update live to reflect each application's state
- `/applications list` - show every configured application, its open/closed state, panel/review channels, accepted roles, and question count
- `/applications panel key:<key> [channel]` - post or refresh the panel for this application's channel (combined if the channel hosts more than one application)
- `/applications setreview key:<key> channel:<#channel>` - where submitted applications go for staff review
- `/applications setpanelchannel key:<key> channel:<#channel>` - where the Apply button panel is posted
- `/applications addrole key:<key> role:<@role>` / `removerole` - roles granted on acceptance
- `/applications setquestions key:<key> questions:<q1|q2|q3>` - replace an application's questions, in order, separated by `|`

**The flow, end to end:**

1. An applicant clicks **Apply** on a panel. The bot DMs them an
   "Application Started" intro, then asks the questions one at a time
   (Appy-style: a `{label} Application` embed with `n/total.` progress and
   a red **Cancel Application** button), waiting for a reply to each - no
   limit on the number of questions, and image/file-only answers are
   captured too. There's a per-question timeout (10 min); clicking Cancel
   Application (or typing **cancel**) stops it. If their DMs are closed
   they get a clear ephemeral note telling them to open DMs and retry;
   only one in-progress interview per user at a time.
2. When the last question is answered, the completed application is posted
   to that type's review channel as an orange-barred embed titled
   `{username}'s '{Label} Application' Application Submitted`, with the
   applicant's avatar as a thumbnail, every question numbered in order, and
   a **Submission stats** field (user id, username, mention, interview
   duration, when they joined the guild, and when they submitted). Staff
   get four buttons: **Accept**, **Deny**, **Accept with reason**, and
   **Deny with reason**. The applicant gets a confirmation in their DMs.
3. **Accept** (staff only) grants the application's configured roles to
   the applicant right away, marks the embed green, retires the buttons,
   and DMs the applicant. **Accept with reason** does the same after a
   short modal for an optional note shared with the applicant.
4. **Deny** (staff only) denies right away with no reason, marks the
   embed red, retires the buttons, and DMs the applicant. **Deny with
   reason** opens a short modal for an optional reason first, then DMs the
   applicant with it.

Reading applicants' DM replies requires the **Direct Messages** gateway
intent (added alongside the existing Message Content intent), so no extra
Developer Portal toggle beyond what the bot already needs.

**Opening and closing:** each application can be opened or closed with
`/applications open`/`close` (pass `all` to do every application at once).
While closed, that application's button on the panel is disabled and
relabelled (on a combined panel only that one button changes; the others
stay active); if someone still manages to click a stale button, the
submission is refused. Reopening re-enables it. New applications default
to open.

**Zero-touch setup for this specific deployment:** if `GUILD_ID` is set,
four applications are seeded automatically on first boot and their panels
are posted once the bot is up, with zero commands required:

| Key | Panel channel | Review channel | Roles on accept | Questions |
|-----|---------------|----------------|-----------------|-----------|
| `gambino` | family panel | Gambino reviews | 3 roles | 7 |
| `colombo` | family panel (same as Gambino) | Colombo reviews | 3 roles | 7 |
| `staff` | staff panel | staff reviews | 1 role | 6 |
| `nypd` | police panel | NYPD reviews | 3 roles | 14 |

Because Gambino and Colombo share the family panel channel, they post as a
single combined panel with two buttons. On boot the bot reconciles any
leftover separate/duplicate panels in a shared channel down to one combined
message, so upgrading from the earlier one-embed-per-app layout cleans
itself up.

Each panel's description is a **REQUIREMENTS** block whose age and
member-time minimums are per-application (`app.minAge` / `app.minMemberTime`,
display-only): Staff is age 15 / member 2 weeks, the crime families are age
14 / member 3 days, and NYPD is age 14 / member 1 week. These are seeded on
a fresh install and backfilled once onto an already-seeded home guild.

As with tickets, this seed runs once, only for that one guild, and never
overwrites an existing configuration; anything else uses `/applications`.

## Police manual

`/police manual setup [channel]` (bot/server owner only) posts a single
static embed, the officer guide & procedures reference, to a channel
(defaults to the channel the command was run in). It's one continuous
orange-barred sheet covering officer conduct, use of force, traffic stops,
vehicle pursuits, felony stops, hostage situations, active shooter
response, and arrest procedures, ending in a final-notes reminder. Nothing
about it is per-guild configurable beyond where it's posted - re-run the
command to post an updated copy or move it to a different channel.

## Data

Runtime state (guild settings, anti-ping config, warnings, muted-role
stashes, snapshots, failsafe backups, ticket config, open-ticket tracking,
application config) lives in `guardian.db`, a
`better-sqlite3` database created automatically on first boot. Legacy
JSON files (`antiping.json`, `mutedroles.json`, `warnings.json`, etc.) are
imported into it once if present, then no longer used. All of these are
git-ignored - they're state, not source. A `security_log.jsonl` forensic
trail is also appended locally for every security event, so it survives a
wiped log channel.

## Development

```bash
npm run check   # syntax check (node --check) for index.js and shard.js
npm run lint    # eslint .
npm test        # node --test - unit tests for config merging, per-guild rate
                 # limits/lockdown isolation (incl. surviving a simulated
                 # restart), ticket + application config isolation, permission
                 # checks, and formatting helpers
```

`index.js` exports its pure/state-only logic (guarded behind
`require.main === module` so `client.login()` never fires when the file is
`require()`d by the test suite) - see the bottom of the file and
`test/*.test.js`. GitHub Actions (`.github/workflows/ci.yml`) runs all
three on every push/PR against Node 20 and 22.

**What's covered by automated tests vs. only by manual testing:** the
per-guild isolation of rate limits/lockdown/config/ticket/application
state (including that it survives a restart), and the permission-hierarchy
logic, are covered by real regression tests. Anything that requires a live
gateway connection - the actual anti-nuke/anti-raid/anti-spam detection
firing against real Discord events, role/channel snapshot-and-restore,
mute-role stashing, the whole ticket flow, and the whole application flow
(Apply button, DM interview, staff accept/deny, role granting) - is not,
and has only been exercised by hand. Treat this
bot as reviewed-and-tested-where-practical, not as verified against live
abuse scenarios at scale.
