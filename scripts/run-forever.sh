#!/usr/bin/env bash
# Keep the paper-trading instrument running.
#
# The measurement is a distribution, and a distribution needs hours. The bot already
# survives a dead WebSocket on its own — it detects a socket that has stopped sending
# without closing, and reconnects. What it cannot survive is the process itself going
# away: an OOM kill, a laptop sleeping, a WSL restart. Losing an overnight run to one of
# those and finding out in the morning is the failure this guards against.
#
# The ledger is append-only and opened fresh on each start, so a restart costs the
# in-flight second and nothing else.
#
#   scripts/run-forever.sh            # foreground, ctrl-C to stop
#   setsid nohup scripts/run-forever.sh >/dev/null 2>&1 &   # detached
#
# Read what it has collected at any time, without stopping it:
#
#   cb-bot --report

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${CB_BOT_BIN:-$HOME/.cargo-target/cryptobot/release/cb-bot}"
LOG="${CB_BOT_LOG:-/tmp/cb-bot.log}"

# A restart loop that retries instantly will spin at full speed against a permanent
# failure — a missing binary, a config error — and bury the real reason under a million
# log lines. Back off, and cap it so a transient outage still recovers promptly.
BACKOFF_MIN=2
BACKOFF_MAX=60
backoff=$BACKOFF_MIN

cd "$ROOT" || exit 1

if [[ ! -x "$BIN" ]]; then
  echo "run-forever: no binary at $BIN" >&2
  echo "             build it first:  cargo build --release -p cb-bot" >&2
  exit 1
fi

echo "run-forever: supervising $BIN"
echo "run-forever: logging to $LOG"

trap 'echo "run-forever: stopping"; kill "${child:-}" 2>/dev/null; exit 0' INT TERM

while true; do
  started=$SECONDS
  "$BIN" >>"$LOG" 2>&1 &
  child=$!
  wait "$child"
  code=$?
  ran=$((SECONDS - started))

  # A process that survived a while was healthy; whatever killed it was probably
  # transient, so reset the backoff. One that dies immediately is failing to start.
  if (( ran > 120 )); then
    backoff=$BACKOFF_MIN
  fi

  echo "run-forever: exited ${code} after ${ran}s, restarting in ${backoff}s" \
    | tee -a "$LOG"
  sleep "$backoff"
  backoff=$(( backoff * 2 ))
  (( backoff > BACKOFF_MAX )) && backoff=$BACKOFF_MAX
done
