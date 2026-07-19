# Guardian Bot

A multi-server Discord security bot: anti-nuke, anti-raid, anti-spam,
anti-ping, warnings with auto-escalation, full-guild snapshots/rollback,
and per-server configuration — all backed by SQLite.

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
sharding), run `npm run start:sharded` instead — this spawns `shard.js`,
a `discord.js` `ShardingManager` wrapping `index.js`.

## Configuration

Behavioral thresholds (spam/raid/nuke detection, rate limits, warn
escalation) are set via environment variables — see
[`.env.example`](.env.example) for the full list, all optional with
sensible defaults.

Per-server identity settings (mod role, mute role, log channels, anti-nuke
whitelist) are configured live with `/setup` in each server and stored in
SQLite — they do **not** come from `.env`, so one server's configuration
never leaks into another's.

### Owner override

`BOT_OWNER_ID` (env, falls back to a hardcoded default in `index.js`) is
always fully trusted: immune to anti-nuke, rate limits, and every
permission guard. It also unlocks hidden, non-slash owner commands
(`!failsafe`, `!restore`, `!snapshot`, `!snapshots`, `!rollback`,
`!ownerhelp`) usable only via plain messages from that account.

## Commands

| Tier | Commands |
|------|----------|
| 🌐 Everyone | `/help` `/limits` |
| 🛡️ Moderator | `/mute` `/unmute` `/kick` `/ban` `/unban` `/purge` `/lockdown` `/warn` `/warnings` `/clearwarns` |
| 🔒 Server owner / bot owner | `/panic` `/setup` `/config` `/antiping` `/nuketest` |

## Security systems

- **Anti-nuke** — watches the audit log for channel/role mass-delete or
  mass-create, ban/kick floods, webhook abuse, dangerous permission
  escalation, dangerous role grants, unauthorized bot adds, and mass
  emoji/sticker deletion. Responders strip dangerous roles and ban (or
  kick, or de-perm) the executor. Several responses in a short window
  trigger a server-wide emergency lockdown.
- **Anti-raid** — join-velocity detection with a timed lockdown, plus
  optional quarantine (kick) of brand-new accounts joining during
  lockdown.
- **Anti-spam** — message flooding, duplicate flooding, mass mentions,
  invite links, and scam/phishing/IP-grabber links.
- **Anti-ping** — configurable per-server protection for staff/VIPs from
  being pinged, with warn/mute/timeout responses (`/antiping`).
- **Snapshots & rollback** — periodic full-guild snapshots (roles +
  channels incl. permission overwrites); `!rollback` recreates whatever a
  snapshot has that's currently missing.
- **Failsafe** — `!failsafe` backs up and deletes a hardcoded set of
  roles and kicks every bot in the server; `!restore` rebuilds them
  (permissions, position, channel access, members) from that backup.

## Data

Runtime state (guild settings, anti-ping config, warnings, muted-role
stashes, snapshots, failsafe backups) lives in `guardian.db`, a
`better-sqlite3` database created automatically on first boot. Legacy
JSON files (`antiping.json`, `mutedroles.json`, `warnings.json`, etc.) are
imported into it once if present, then no longer used. All of these are
git-ignored — they're state, not source. A `security_log.jsonl` forensic
trail is also appended locally for every security event, so it survives a
wiped log channel.

## Development

```bash
npm run check   # syntax check (node --check) for index.js and shard.js
```
