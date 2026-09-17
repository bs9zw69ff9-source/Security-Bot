# Guardian Bot

The bot crate: [serenity](https://github.com/serenity-rs/serenity) 0.12 +
tokio + rusqlite. See the [root README](../README.md) for what the bot
actually does; this file covers building and running it.

This started life as a Node.js bot, which is why the persisted schema and
several in-code comments still reference it. That implementation has been
removed, but the `guardian.db` format is unchanged, so an existing database
from the old bot is picked up as-is with nothing to migrate.

## Running it

```bash
cd rust
cargo build --release
./target/release/guardian-bot
```

`dotenvy` walks up from the working directory and picks up the repo-root
`.env`; `DISCORD_TOKEN` is the only value it truly needs. State files resolve
to the crate's **parent** directory, so they sit at the repo root:

| File | Purpose |
|------|---------|
| `../guardian.db` | all persisted state (override with `GUARDIAN_DB_FILE`) |
| `../security_log.jsonl` | forensic trail, appended on every security event |
| `../antiping.json`, `../warnings.json`, … | legacy JSON, imported once if present |

Those paths are resolved from `CARGO_MANIFEST_DIR` at compile time, so the
binary writes to the same place regardless of the directory you launch it
from. Copying the binary to a host that lacks the repo at that path means
setting `GUARDIAN_DB_FILE`, or rebuilding on the target.

Boot order is database first, token second: `db::init()` and the seed
migrations run before the token check, so even a bad token leaves you with a
correctly initialized `guardian.db`. A missing token exits 1 with
`❌ DISCORD_TOKEN is not set.`; a rejected one reports `401: Unauthorized`.
Ctrl-C shuts the shards down cleanly.

Run only **one** process against a given database at a time - it keeps an
authoritative in-memory copy, so two would overwrite each other's writes.

## Layout

- `src/common/` - low-level shared pieces: `config` (env + constants), `db`
  (SQLite + forensic log), `client`, `embeds` (colors, builders, `sec_log`,
  `alert_owner`), `permissions`, and `guildinfo`
- `src/state/` - per-guild config get/set plus the one-time home-guild seeds
  and question backfills, one module per persisted table
- `src/systems/` - feature logic: mute, anti-spam, anti-ping, anti-raid,
  anti-nuke, snapshot/rollback, failsafe, message logging, hidden owner
  commands, `/setup` helpers, tickets, applications, police manual, and
  chain-of-command boards
- `src/commands/` - slash command definitions and the dispatch handler
- `src/main.rs` - the `EventHandler`, boot sequence, and background timers

`src/common/guildinfo.rs` exists because serenity's cache hands back a guard
that cannot be held across an `.await`; anything needing role positions or
permissions *and* making API calls copies what it needs out of the cache
first via `GuildInfo`.

## Implementation notes

- **Timers.** `tokio::time::sleep` takes a real `Duration`, so even a
  month-long mute just sleeps the whole span in one task. No chunking.
- **Debounced chain-of-command refresh.** Each scheduled refresh carries a
  generation number and only the newest one renders, so a burst of role
  changes collapses into a single re-render without juggling timer handles.
- **Deleted-message content.** Showing what a deleted message said depends on
  the message cache, so anything sent before the last restart falls back to
  *"content not cached (sent before restart)"*.
- **`/status`** reports RSS from `/proc/self/statm` and gateway latency from
  the shard runner via a `SHARD_MANAGER` handle stashed at boot.

## Development

```bash
cargo check --all-targets   # type check
cargo clippy --all-targets  # lints (currently clean)
cargo test                  # unit tests
cargo build --release
```

The tests cover two things. The anti-nuke counters: that an attack rotating
through categories trips the shared aggregate counter rather than slipping
under every per-category limit, and that a single-category burst still trips
its own. And the police-manual text, asserted at exactly 3667 UTF-16 units,
which is the unit Discord's 4096 embed-description limit counts in.
