#!/usr/bin/env bash
#
# Pull, build, and (re)start the bot.
#
#   ./deploy.sh            pull, build, restart
#   ./deploy.sh restart    build and restart, no pull
#   ./deploy.sh start      start only if it isn't already running
#   ./deploy.sh stop       stop it
#   ./deploy.sh status     is it running, and on which commit
#   ./deploy.sh logs       tail the log
#
# If a systemd unit named guardian-bot exists, start/stop/restart are handed
# to systemctl and the PID-file path below is not used.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$ROOT/rust/target/release/guardian-bot"
PIDFILE="$ROOT/guardian-bot.pid"
LOGFILE="$ROOT/guardian-bot.log"
UNIT="guardian-bot"

# SIGTERM, not SIGINT. The bot handles both (see wait_for_shutdown_signal in
# rust/src/main.rs), but SIGINT is the wrong tool from a script: when job
# control is off, which it is in any non-interactive shell, bash starts
# background commands with SIGINT set to SIG_IGN, and that disposition
# survives exec. SIGTERM carries no such baggage and is what every other
# supervisor sends anyway.
STOP_SIGNAL="TERM"
STOP_TIMEOUT=15

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m==>\033[0m %s\n' "$*" >&2; exit 1; }

# Prefer systemd when a unit is actually installed, so we don't end up with
# two supervisors fighting over the same process.
use_systemd() {
  command -v systemctl >/dev/null 2>&1 &&
    systemctl list-unit-files "$UNIT.service" >/dev/null 2>&1 &&
    [ -n "$(systemctl list-unit-files "$UNIT.service" --no-legend 2>/dev/null)" ]
}

# True if the PID is a live guardian-bot.
#
# Checks the command name, so a recycled PID can't make us signal a stranger,
# and rejects zombies: a process that has exited but not yet been reaped still
# answers `kill -0`, which would otherwise read as "still shutting down" and
# burn the whole stop timeout before a pointless SIGKILL.
alive() {
  local pid="${1:-}"
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  local info
  info="$(ps -p "$pid" -o stat=,comm= 2>/dev/null || true)"
  case "$info" in
    Z*|"") return 1 ;;
    *"guardian-bot") return 0 ;;
    *) return 1 ;;
  esac
}

# Echo the running PID, or nothing.
running_pid() {
  [ -f "$PIDFILE" ] || return 0
  local pid
  pid="$(cat "$PIDFILE" 2>/dev/null || true)"
  alive "$pid" && echo "$pid"
  return 0
}

do_pull() {
  local branch
  branch="$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)"

  if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
    die "Working tree has uncommitted changes. Commit or stash them, then re-run."
  fi

  say "Pulling $branch"
  local delay=2
  for attempt in 1 2 3 4 5; do
    if git -C "$ROOT" pull --ff-only origin "$branch"; then
      return 0
    fi
    [ "$attempt" -eq 5 ] && die "git pull failed after 5 attempts."
    warn "Pull failed, retrying in ${delay}s"
    sleep "$delay"
    delay=$((delay * 2))
  done
}

do_build() {
  say "Building release"
  # Deliberately built before anything is stopped: a broken commit should
  # leave the running bot untouched rather than take it down with nothing to
  # put back.
  ( cd "$ROOT/rust" && cargo build --release ) || die "Build failed. The running bot was left alone."
}

do_stop() {
  if use_systemd; then
    say "Stopping $UNIT via systemd"
    sudo systemctl stop "$UNIT"
    return
  fi

  local pid
  pid="$(running_pid)"
  if [ -z "$pid" ]; then
    say "Not running"
    rm -f "$PIDFILE"
    return
  fi

  say "Stopping (pid $pid, SIG$STOP_SIGNAL)"
  kill -"$STOP_SIGNAL" "$pid" 2>/dev/null || true

  for _ in $(seq "$STOP_TIMEOUT"); do
    alive "$pid" || break
    sleep 1
  done

  if alive "$pid"; then
    warn "Still up after ${STOP_TIMEOUT}s, sending SIGKILL"
    kill -9 "$pid" 2>/dev/null || true
    sleep 1
  fi

  rm -f "$PIDFILE"
  say "Stopped"
}

do_start() {
  if use_systemd; then
    say "Starting $UNIT via systemd"
    sudo systemctl start "$UNIT"
    sleep 2
    systemctl status "$UNIT" --no-pager --lines=10 || true
    return
  fi

  local pid
  pid="$(running_pid)"
  if [ -n "$pid" ]; then
    say "Already running (pid $pid)"
    return
  fi

  [ -x "$BIN" ] || die "No binary at $BIN. Run './deploy.sh' or 'cargo build --release' first."

  say "Starting"
  # Launched from the repo root so dotenvy finds .env sitting right there.
  #
  # stdin comes from /dev/null on purpose: a daemon that inherits the calling
  # shell's stdin keeps it open, which leaves whatever invoked this script
  # (a terminal, CI, another script) looking like it hung.
  #
  # Backgrounded as a plain command rather than inside a subshell, so $! is
  # the bot's own PID. Wrapping it in ( ... ) would hand back the subshell's
  # PID instead, and the pidfile would point at a process that is already gone.
  cd "$ROOT"
  if command -v setsid >/dev/null 2>&1; then
    setsid "$BIN" </dev/null >>"$LOGFILE" 2>&1 &
  else
    nohup "$BIN" </dev/null >>"$LOGFILE" 2>&1 &
  fi
  echo $! >"$PIDFILE"

  sleep 3
  pid="$(running_pid)"
  if [ -z "$pid" ]; then
    rm -f "$PIDFILE"
    warn "It exited immediately. Last lines of $LOGFILE:"
    tail -n 20 "$LOGFILE" >&2 || true
    exit 1
  fi
  say "Running (pid $pid), logging to $LOGFILE"
}

do_status() {
  say "Commit: $(git -C "$ROOT" log --oneline -1)"

  if use_systemd; then
    systemctl status "$UNIT" --no-pager --lines=10 || true
    return
  fi

  local pid
  pid="$(running_pid)"
  if [ -n "$pid" ]; then
    say "Running (pid $pid)"
    ps -p "$pid" -o pid,etime,rss,cmd --no-headers || true
  else
    say "Not running"
  fi
}

case "${1:-deploy}" in
  deploy)  do_pull; do_build; do_stop; do_start ;;
  restart) do_build; do_stop; do_start ;;
  start)   do_start ;;
  stop)    do_stop ;;
  status)  do_status ;;
  logs)    tail -f "$LOGFILE" ;;
  *)       die "Unknown command '$1'. Try: deploy, restart, start, stop, status, logs" ;;
esac
