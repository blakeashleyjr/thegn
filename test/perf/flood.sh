#!/usr/bin/env bash
# Input/switch latency under a multi-pane PTY flood ("keystroke during flood").
#
# Launches szhost in a PTY over the worktree fixture (like cpu-sample.sh),
# starts a full-speed output flood (`seq`) in the shells of several worktrees,
# then fires a burst of worktree switches (Alt+Down) mid-flood and reads the
# perf rollup's switch/input/drain/flush percentiles from the szhost log.
#
# Advisory (machine-dependent) — NOT a CI gate. Use it to capture before/after
# evidence for loop-scheduling changes: the numbers to watch are
# switch_p99_us (worktree-switch → first frame) and input_p99_us (keystroke →
# frame) staying under SUPERZEJ_INPUT_BUDGET_US while pty_bytes_per_s is high.
#
# Usage: flood.sh [--bin PATH] [--worktrees N] [--floods N] [--switches N] [--json]
#
# Exit status: 0 ok (numbers printed); 1 harness error.

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=test/perf/lib/env.sh disable=SC1091
source "$HERE/lib/env.sh"
# shellcheck source=test/perf/lib/fixture.sh disable=SC1091
source "$HERE/lib/fixture.sh"

BIN="${SZ_PERF_BIN:-target/release/szhost}"
WORKTREES="${SZ_PERF_WORKTREES:-8}"
FLOODS=3    # how many worktrees' shells run a flood
SWITCHES=30 # Alt+Down burst size, fired mid-flood
JSON_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
  --bin)
    BIN="$2"
    shift 2
    ;;
  --worktrees)
    WORKTREES="$2"
    shift 2
    ;;
  --floods)
    FLOODS="$2"
    shift 2
    ;;
  --switches)
    SWITCHES="$2"
    shift 2
    ;;
  --json)
    JSON_ONLY=1
    shift
    ;;
  *)
    echo "flood: unknown arg: $1" >&2
    exit 1
    ;;
  esac
done

BIN_ABS="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
[ -x "$BIN_ABS" ] || {
  echo "flood: binary not executable: $BIN_ABS" >&2
  exit 1
}
case "$BIN_ABS" in
*target/release/*) BUILD=release ;;
*target/debug/*) BUILD=debug ;;
*) BUILD=unknown ;;
esac
GIT_SHA="$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo unknown)"

perf_make_tmp
perf_trap_cleanup
HOST_TAG="$(perf_host_tag)"
REPO="$(perf_build_fixture "$WORKTREES" 0)"

command -v script >/dev/null 2>&1 || {
  echo "flood: script(1) not found" >&2
  exit 1
}

PIDFILE="$PERF_TMP/szhost.pid"
# settle + (per-flood: type+switch) + burst + rollup tail. The perf interval is
# forced low so at least two rollups land inside the window. Big-fixture runs
# need a longer settle (startup hydration of N worktrees), or the first typed
# keys land mid-storm and pollute the input percentiles.
SETTLE_MS="${SZ_PERF_SETTLE_MS:-3000}"
BURST_GAP_MS=120
RUN_MS=$((SETTLE_MS + FLOODS * 800 + SWITCHES * BURST_GAP_MS + 4000))
DEADLINE_S=$(((RUN_MS / 1000) + 15))

printf -v INNER \
  'cd %q; stty rows 50 cols 200; env SUPERZEJ_BENCH_RUN_MS=%q SUPERZEJ_PERF=1 SUPERZEJ_PERF_INTERVAL_MS=2000 SUPERZEJ_LOG=szhost::perf=debug %q & echo $! > %q; wait' \
  "$REPO" "$RUN_MS" "$BIN_ABS" "$PIDFILE"
# Key injection goes through script(1)'s STDIN (it forwards to the pty
# MASTER). Writing to szhost's /proc fd/0 would hit the pty *slave* — that's
# pane output, not input. A held-open FIFO keeps script from seeing EOF.
FIFO="$PERF_TMP/keys.fifo"
mkfifo "$FIFO"
timeout "${DEADLINE_S}s" script -qec "$INNER" /dev/null <"$FIFO" >/dev/null 2>&1 &
LAUNCHER=$!
exec 3>"$FIFO"

for _ in $(seq 1 100); do
  [ -s "$PIDFILE" ] && break
  sleep 0.05
done
PID="$(cat "$PIDFILE" 2>/dev/null || true)"
[ -n "$PID" ] && [ -d "/proc/$PID" ] || {
  echo "flood: szhost did not start" >&2
  kill "$LAUNCHER" 2>/dev/null || true
  exit 1
}

keys() { # write raw bytes to the outer pty master via script's stdin
  printf '%b' "$1" >&3 2>/dev/null || true
}

sleep "$(awk "BEGIN{print $SETTLE_MS/1000}")"

# Dismiss the first-launch keymap-preset dialog (fresh XDG state every run) so
# the burst keys reach the compositor's keymap, not a modal.
keys '\r'
sleep 0.3

# Start a flood in the focused shells of $FLOODS worktrees: type the command,
# Enter, then Alt+Down to the next worktree. `seq` is pure scroll — alacritty's
# worst case — and keeps producing in the background after we switch away.
for _ in $(seq 1 "$FLOODS"); do
  keys 'seq 1 100000000\r'
  sleep 0.4
  keys '\x1b[1;3B' # Alt+Down → NextWorktree
  sleep 0.4
done

# Mid-flood switch burst: the measurement the harness exists for.
for _ in $(seq 1 "$SWITCHES"); do
  keys '\x1b[1;3B'
  sleep "$(awk "BEGIN{print $BURST_GAP_MS/1000}")"
done

exec 3>&- # close the key channel; szhost exits on its bench window
wait "$LAUNCHER" 2>/dev/null || true

LOG="$XDG_STATE_HOME/superzej/logs/szhost.log"
[ -f "$LOG" ] || {
  echo "flood: no szhost log at $LOG" >&2
  exit 1
}

# Pull every rollup's fields; report the WORST interval (the one that saw the
# burst) so a lucky quiet rollup can't mask a regression. The log subscriber
# styles field names with SGR escapes — strip them before matching, and never
# fail the pipeline on a missing field (pipefail + set -e).
extract() { # $1 = field name -> max value across rollups
  { sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -o "${1}=[0-9.]*" | cut -d= -f2 | sort -rn | head -1; } || true
}
ROLLUPS="$(grep -c 'perf rollup' "$LOG" || true)"
[ "${ROLLUPS:-0}" -gt 0 ] || {
  echo "flood: no perf rollups captured (run too short?)" >&2
  exit 1
}
SWITCH_P50="$(extract switch_p50_us)"
SWITCH_P99="$(extract switch_p99_us)"
INPUT_P50="$(extract input_p50_us)"
INPUT_P99="$(extract input_p99_us)"
DRAIN_P99="$(extract drain_p99_us)"
FLUSH_P99="$(extract flush_p99_us)"
RENDER_P99="$(extract render_p99_us)"
PTY_BPS="$(extract pty_bytes_per_s)"

RESULT="{\"scenario\":\"flood\",\"build\":\"$BUILD\",\"worktrees\":$WORKTREES,\"floods\":$FLOODS,\"switches\":$SWITCHES,\"switch_p50_us\":${SWITCH_P50:-0},\"switch_p99_us\":${SWITCH_P99:-0},\"input_p50_us\":${INPUT_P50:-0},\"input_p99_us\":${INPUT_P99:-0},\"drain_p99_us\":${DRAIN_P99:-0},\"flush_p99_us\":${FLUSH_P99:-0},\"render_p99_us\":${RENDER_P99:-0},\"pty_bytes_per_s\":${PTY_BPS:-0},\"rollups\":$ROLLUPS,\"git_sha\":\"$GIT_SHA\",\"host_tag\":\"$HOST_TAG\"}"

if [ "$JSON_ONLY" = 1 ]; then
  printf '%s\n' "$RESULT"
else
  echo "scenario=flood build=$BUILD worktrees=$WORKTREES floods=$FLOODS switches=$SWITCHES (sha=$GIT_SHA host=$HOST_TAG)"
  echo "  switch→frame  p50=${SWITCH_P50:-–}us  p99=${SWITCH_P99:-–}us   (worst rollup)"
  echo "  input→frame   p50=${INPUT_P50:-–}us  p99=${INPUT_P99:-–}us"
  echo "  drain p99=${DRAIN_P99:-–}us  flush p99=${FLUSH_P99:-–}us  render p99=${RENDER_P99:-–}us"
  echo "  pty_bytes_per_s=${PTY_BPS:-0}  rollups=$ROLLUPS"
fi
