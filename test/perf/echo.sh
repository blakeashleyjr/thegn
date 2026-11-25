#!/usr/bin/env bash
# Quiet-session keystroke echo latency (speed claim SP-7).
#
# Runs one multiplexer in a PTY over the perf fixture, sends Esc (dismisses the
# first-launch wizard for thegn), starts `cat` in the focused shell, then types
# N keystrokes at a fixed spacing (default 200 keys at 20 ms). For each key it
# records the first PTY output chunk that arrives after the write, and reports
# p50/p95/p99 over those samples. Driver: test/perf/lib/echo_pty.py.
#
# SUBMISSION, NOT PAINT. The number is the time from writing a key to the
# multiplexer writing output back to its PTY. It does not measure when pixels
# reached the screen, and it is not a claim that the application responded.
# Read it next to docs/performance-benchmark-protocol.md, not as a display
# latency.
#
# Tools: thegn (default) and tmux. zellij is not wired yet (SP-2 compare work).
# The tmux arm runs `cat` as the pane command, so it skips the thegn-only
# wizard dismissal and `cat` start.
#
# Advisory and machine-dependent — NOT a CI gate. Run on the remote Mac via
# `just perf-echo`; numbers from a loaded laptop are not comparable.
#
# Usage: echo.sh [--bin PATH] [--tool thegn|tmux] [--keys N] [--spacing-ms MS] [--json]
# Exit status: 0 numbers printed; 1 harness error.

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=test/perf/lib/env.sh disable=SC1091
source "$HERE/lib/env.sh"
# shellcheck source=test/perf/lib/fixture.sh disable=SC1091
source "$HERE/lib/fixture.sh"

BIN="${TG_PERF_BIN:-target/release/thegn}"
TOOL=thegn
KEYS=200
SPACING_MS=20
JSON_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
  --bin)
    BIN="$2"
    shift 2
    ;;
  --tool)
    TOOL="$2"
    shift 2
    ;;
  --keys)
    KEYS="$2"
    shift 2
    ;;
  --spacing-ms)
    SPACING_MS="$2"
    shift 2
    ;;
  --json)
    JSON_ONLY=1
    shift
    ;;
  *)
    echo "echo: unknown arg: $1" >&2
    exit 1
    ;;
  esac
done

case "$TOOL" in
thegn | tmux) ;;
*)
  echo "echo: --tool must be thegn or tmux (got: $TOOL)" >&2
  exit 1
  ;;
esac
command -v python3 >/dev/null 2>&1 || {
  echo "echo: python3 not found (needed for the PTY driver)" >&2
  exit 1
}
GIT_SHA="$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo unknown)"

# Run the multiplexer from the perf environment, not the caller's tmux session:
# a TMUX variable from an outer tmux makes `tmux new-session` refuse to start.
unset TMUX

perf_make_tmp
perf_sandbox_off_default
perf_trap_cleanup
HOST_TAG="$(perf_host_tag)"
REPO="$(perf_build_fixture 1 0)"

SETTLE_MS="${TG_PERF_SETTLE_MS:-3000}"

BIN_ABS=""
BUILD=unknown
PRELUDE=()
if [ "$TOOL" = thegn ]; then
  BIN_ABS="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
  [ -x "$BIN_ABS" ] || {
    echo "echo: binary not executable: $BIN_ABS" >&2
    exit 1
  }
  case "$BIN_ABS" in
  *target/release/*) BUILD=release ;;
  *target/debug/*) BUILD=debug ;;
  esac
  # The bench window bounds the run; the driver kills thegn when it is done.
  RUN_MS=$((SETTLE_MS + 500 + KEYS * SPACING_MS + 5000))
  # Esc dismisses the first-launch wizard (Enter would advance it), then `cat`
  # becomes the focused pane's foreground process so keys echo through its tty.
  PRELUDE=(--prelude '\x1b' --prelude 'cat\r')
  printf -v CHILD 'cd %q; exec env THEGN_BENCH_RUN_MS=%q %q' "$REPO" "$RUN_MS" "$BIN_ABS"
else
  command -v tmux >/dev/null 2>&1 || {
    echo "echo: tmux not found (--tool tmux)" >&2
    exit 1
  }
  BUILD="$(tmux -V 2>/dev/null | tr -d ' ' || echo unknown)"
  TMUX_SOCK="$PERF_TMP/tmux.sock"
  # The driver's PTY is the tmux client. Kill the private server on exit so a
  # failed run cannot leave a tmux server behind.
  trap 'tmux -f /dev/null -S "$TMUX_SOCK" kill-server >/dev/null 2>&1 || true; perf_cleanup' EXIT
  printf -v CHILD 'cd %q; exec tmux -f /dev/null -S %q new-session -s echo cat' \
    "$REPO" "$TMUX_SOCK"
fi

JSON_FLAG=()
[ "$JSON_ONLY" = 1 ] && JSON_FLAG=(--json)
RESULT="$(python3 -I "$HERE/lib/echo_pty.py" \
  --child "$CHILD" \
  --keys "$KEYS" \
  --spacing-ms "$SPACING_MS" \
  --settle-ms "$SETTLE_MS" \
  --tool "$TOOL" \
  --build "$BUILD" \
  --sha "$GIT_SHA" \
  --host "$HOST_TAG" \
  ${PRELUDE[@]+"${PRELUDE[@]}"} ${JSON_FLAG[@]+"${JSON_FLAG[@]}"})" || {
  echo "echo: PTY driver failed for tool=$TOOL" >&2
  exit 1
}

printf '%s\n' "$RESULT"
