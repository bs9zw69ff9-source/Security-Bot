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
SELF="$ROOT/$(basename "${BASH_SOURCE[0]}")"
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
  branch="$(as_owner git -C "$ROOT" rev-parse --abbrev-ref HEAD)"

  if [ -n "$(as_owner git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
    die "Working tree has uncommitted changes. Commit or stash them, then re-run."
  fi

  say "Pulling $branch"
  local before after delay=2
  before="$(cksum < "$SELF" 2>/dev/null || true)"
  for attempt in 1 2 3 4 5; do
    if as_owner git -C "$ROOT" pull --ff-only origin "$branch"; then
      after="$(cksum < "$SELF" 2>/dev/null || true)"
      # If that pull rewrote this script, finish the run in the new copy.
      # Carrying on would run a mix of both: bash reads a script by byte
      # offset as it goes, so the rest of this file is no longer where the
      # running process thinks it is.
      if [ "$before" != "$after" ] && [ -z "${DEPLOY_RELOADED:-}" ]; then
        say "deploy.sh changed in that pull, handing over to the new version"
        DEPLOY_RELOADED=1 exec "$SELF" "${ACTION}"
      fi
      return 0
    fi
    [ "$attempt" -eq 5 ] && die "git pull failed after 5 attempts."
    warn "Pull failed, retrying in ${delay}s"
    sleep "$delay"
    delay=$((delay * 2))
  done
}

# Locate cargo without relying on the caller's PATH.
#
# rustup installs per user, into ~/.cargo/bin, and adds that to PATH from the
# shell profile. Anything that does not load a profile - sudo, cron, systemd, a
# root shell when rustup was installed as someone else - sees no cargo at all.
#
# When the build is going to run as the checkout's owner, it is *their* cargo
# that matters. Ours might be on PATH and still be unusable: root's copy under
# /root is not even readable by a service account.
find_cargo() {
  local owner me_home owner_home candidate
  owner="$(repo_owner)"
  owner_home=""
  [ -n "$owner" ] && owner_home="$(getent passwd "$owner" 2>/dev/null | cut -d: -f6 || true)"
  me_home="$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f6 || true)"

  if [ -n "$owner" ] && [ "$(id -un)" != "$owner" ] && [ -n "$owner_home" ] \
     && [ -x "$owner_home/.cargo/bin/cargo" ]; then
    echo "$owner_home/.cargo/bin/cargo"
    return 0
  fi

  if command -v cargo >/dev/null 2>&1; then
    command -v cargo
    return 0
  fi

  for candidate in \
    "${CARGO_HOME:-}/bin/cargo" \
    "$me_home/.cargo/bin/cargo" \
    "$owner_home/.cargo/bin/cargo" \
    "${HOME:-}/.cargo/bin/cargo" \
    /root/.cargo/bin/cargo \
    /usr/local/cargo/bin/cargo \
    /usr/local/bin/cargo
  do
    case "$candidate" in /bin/cargo|"/.cargo/bin/cargo") continue ;; esac
    if [ -x "$candidate" ]; then
      echo "$candidate"
      return 0
    fi
  done
  return 1
}

# Who owns the checkout. Everything that writes into it runs as them, so a
# root-driven deploy never leaves root-owned files behind for the service
# account to trip over.
repo_owner() {
  stat -c '%U' "$ROOT" 2>/dev/null || true
}

# Run a command as the checkout's owner. A no-op when we already are them.
#
# runuser is preferred over sudo: root using sudo to become a passwordless
# system account still goes through PAM, and on some configurations that
# prompts for a password the account does not have. runuser is built for
# exactly this transition and never prompts. -H / -l style home handling
# matters too, since cargo writes to $HOME/.cargo and would otherwise try to
# use root's.
as_owner() {
  local owner
  owner="$(repo_owner)"
  if [ -z "$owner" ] || [ "$(id -un)" = "$owner" ]; then
    "$@"
    return
  fi
  if [ "$(id -u)" -ne 0 ]; then
    die "This checkout belongs to '$owner'. Re-run as root or as $owner."
  fi
  local home
  home="$(getent passwd "$owner" 2>/dev/null | cut -d: -f6 || true)"
  if command -v runuser >/dev/null 2>&1; then
    HOME="${home:-$HOME}" runuser -u "$owner" -- "$@"
  else
    sudo -H -u "$owner" -- "$@"
  fi
}

# systemctl needs root. Only reach for sudo when we are not already root,
# because a passwordless service account cannot answer a sudo prompt.
systemctl_root() {
  if [ "$(id -u)" -eq 0 ]; then
    systemctl "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo systemctl "$@"
  else
    die "Need root to run 'systemctl $*'. Re-run this as root."
  fi
}

do_build() {
  local cargo
  cargo="$(find_cargo)" || die "Couldn't find cargo. Install Rust for this account (https://rustup.rs), or set CARGO_HOME if it lives somewhere unusual."

  say "Building release (${cargo})"
  # Deliberately built before anything is stopped: a broken commit should
  # leave the running bot untouched rather than take it down with nothing to
  # put back.
  ( cd "$ROOT/rust" && as_owner "$cargo" build --release ) || die "Build failed. The running bot was left alone."
}

do_stop() {
  if use_systemd; then
    say "Stopping $UNIT via systemd"
    systemctl_root stop "$UNIT"
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
    systemctl_root start "$UNIT"
    sleep 2
    systemctl_root status "$UNIT" --no-pager --lines=10 || true
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
  # Created up front as the owner: the redirect below is opened by this shell,
  # so a root-driven run would otherwise leave a root-owned log and pidfile
  # that the service account cannot write on its next start.
  as_owner touch "$LOGFILE" "$PIDFILE" 2>/dev/null || true
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
  say "Commit: $(as_owner git -C "$ROOT" log --oneline -1)"

  if use_systemd; then
    systemctl_root status "$UNIT" --no-pager --lines=10 || true
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

# Everything runs from here, called on the last line.
#
# Bash reads a script incrementally as it executes, so a `git pull` that
# rewrites this file mid-run leaves the process reading the rest of it from a
# byte offset that no longer means anything. Keeping the whole body inside a
# function forces bash to parse the entire file before executing any of it,
# which makes the running copy immune to being replaced underneath it.
# Hand the whole run to the checkout's owner when root is not needed.
#
# Without a systemd unit this script supervises the bot itself, and that only
# works while it *is* the service account: the pidfile has to hold the bot's
# own pid, and a root-launched daemon would run as root. With a unit installed
# we stay root, because systemctl needs it, and hand the git and cargo work
# over individually instead.
maybe_drop_privileges() {
  [ "$(id -u)" -eq 0 ] || return 0
  [ -z "${DEPLOY_DROPPED:-}" ] || return 0
  local owner home
  owner="$(repo_owner)"
  [ -n "$owner" ] && [ "$owner" != "root" ] || return 0
  use_systemd && return 0

  home="$(getent passwd "$owner" 2>/dev/null | cut -d: -f6 || true)"
  say "No systemd unit here, so running the whole thing as $owner"
  export DEPLOY_DROPPED=1
  if command -v runuser >/dev/null 2>&1; then
    HOME="${home:-$HOME}" exec runuser -u "$owner" -- "$SELF" "$ACTION"
  else
    exec sudo -H -u "$owner" -- "$SELF" "$ACTION"
  fi
}

main() {
  ACTION="${1:-deploy}"
  maybe_drop_privileges
  case "$ACTION" in
    deploy)  do_pull; do_build; do_stop; do_start ;;
    restart) do_build; do_stop; do_start ;;
    start)   do_start ;;
    stop)    do_stop ;;
    status)  do_status ;;
    logs)    tail -f "$LOGFILE" ;;
    *)       die "Unknown command '$ACTION'. Try: deploy, restart, start, stop, status, logs" ;;
  esac
}

main "$@"
