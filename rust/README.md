# Guardian Bot - Rust port

A port of the Node.js bot in the parent directory to Rust
([serenity](https://github.com/serenity-rs/serenity) 0.12 + tokio +
rusqlite). Feature-for-feature equivalent: same commands, same embeds, same
thresholds, same customIds, and the **same `guardian.db` schema** - the two
implementations read and write byte-identical rows, so you can switch between
them without migrating anything.

## Running it

```bash
cd rust
cargo build --release
DISCORD_TOKEN=... ./target/release/guardian-bot
```

It reads the same `.env` as the JS bot (`dotenvy` loads it), and the same
state files, because paths resolve to the **parent** directory:

| File | Purpose |
|------|---------|
| `../guardian.db` | all persisted state (override with `GUARDIAN_DB_FILE`) |
| `../security_log.jsonl` | forensic trail, appended on every security event |
| `../antiping.json`, `../warnings.json`, … | legacy JSON, imported once if present |

Only run **one** of the two implementations against a given database at a
time - they both keep authoritative in-memory copies, so running both
concurrently would let one overwrite the other's writes.

## Layout

Mirrors the JS module split one-to-one:

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

## Notable differences from the JS version

These are consequences of the runtime, not behaviour changes:

- **Timers.** JS `setTimeout` silently overflows past ~24.8 days, so the JS
  bot chunked long delays. `tokio::time::sleep` takes a real `Duration`, so a
  long mute just sleeps the whole span in one task.
- **Debounced chain-of-command refresh.** JS cleared and reset a timer
  handle; here each scheduled refresh carries a generation number and only
  the newest one renders. Same effect, no handle juggling.
- **Deleted-message content.** Both versions depend on a message cache to
  show what a deleted message said, and both fall back to
  *"content not cached (sent before restart)"*.
- **`/status`** reports RSS from `/proc/self/statm` and gateway latency from
  the shard runner, rather than `process.memoryUsage()` / `client.ws.ping`.

## Development

```bash
cargo check     # type check
cargo clippy    # lints (currently clean)
cargo test      # unit tests
cargo build --release
```

`cargo test` covers the police-manual text, which is asserted to be exactly
3667 UTF-16 units - identical to what the JS implementation produced, and
under Discord's 4096 embed-description limit.
