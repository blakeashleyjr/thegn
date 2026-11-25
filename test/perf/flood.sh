#!/usr/bin/env bash
# Input/switch latency under a multi-pane PTY flood ("keystroke during flood").
#
# Launches thegn in a PTY over the worktree fixture (like cpu-sample.sh), starts
# a full-speed output flood, then fires a burst of worktree switches (Alt+Down)
# mid-flood and reads the perf rollup's switch/input/drain/flush percentiles
# from the thegn log. The harness captures script(1)'s typescript so it can
# assert on the rendered output, not just the numbers.
#
# Modes (--mode):
#   seq         `seq` scroll flood in the focused shells of --floods worktrees
#               (the original fixture; pure scroll, no per-byte payload).
#   cat-file    unbounded `cat` loop of a generated big file (default 4 MiB,
#               TG_PERF_CAT_BYTES) in the same worktree-shell layout: heavy
#               output with real byte volume, like a build log dumping away.
#   many-panes  builds --panes panes in the active worktree via Alt+p smart
#               splits — 16 per tab, mirroring the host's
#               MAX_PANES_PER_TAB (handlers/worktree_attach.rs); extra panes
#               spill into Alt+t tabs — floods --floods of them with the same
#               `cat` loop, and leaves the last pane quiet. Record the pane
#               sweep with e.g. `--mode many-panes --panes 16` and
#               `--panes 64`.
#
# Correctness (THE-2662): every mode types a pre-marker plus N echo markers
# (TG_PERF_MARKERS, default 6) into one QUIET pane mid-flood and, after the
# run, asserts:
#   (a) the marker file is byte-exact and in order — each command appends one
#       line to $HOME/quiet.log, and the marker text is quoted with no spaces
#       around the redirect, so every realistic dropped-key outcome corrupts
#       a line: a dropped `"` leaves bash waiting for its match (the next
#       command lands inside the string), a dropped `>` or `;` misroutes the
#       echo into the pane or merges it into the line, a dropped Enter
#       concatenates two markers into one corrupted line, and the pre-marker
#       means a truncating `>` always destroys an earlier line. (Residual
#       hole: a lone `>` dropped on the pre-marker itself truncates an empty
#       file and is invisible.) This is the ordering/no-dropped-keys gate for
#       typed input under flood.
#   (b) at least one marker reached the rendered output stream (grep the
#       typescript) — quiet-pane output still arrives while the flooders
#       saturate the pty readers.
# Latency numbers are only reported as passing when both hold; the harness
# exits 1 otherwise. The cat flood's own byte volume is reported (accounted
# pty bytes, per-flooder cat bytes) but not gated: its producer is an
# unbounded loop, so "produced == accounted" has no exact count to hold
# against — the byte-exact gate is the quiet pane's.
#
# p99 reporting: each 2s rollup carries its own windowed percentiles, so a
# lucky quiet rollup can mask a regression. The harness reports BOTH the
# worst (max) per-rollup p99 — the old behaviour — and a p99 pooled across
# all rollups (nearest-rank over the per-rollup values; with few rollups
# that equals the worst, so the rollup count is printed too). Field matching
# is anchored on the preceding space: a bare `input_p99_us=` would also match
# inside `writer_input_p99_us=`, silently reporting the writer's completion
# tail instead of the input→frame latency.
#
# Advisory (machine-dependent) — NOT a CI gate. Use it to capture before/after
# evidence for loop-scheduling changes: the numbers to watch are
# switch_p99_us (worktree-switch → first frame) and input_p99_us (keystroke →
# frame) staying under the proposed budgets — input 50 ms, switch 250 ms
# (docs/performance-benchmark-protocol.md) — while pty_bytes_per_s is high.
# A breach is "file a perf bug and soften the claim", not a harness failure.
#
# Tab switches (Alt+Right = nav-right, next tab) are fired in a second burst
# after the worktree burst. Both feed the same switch_* histograms (the rollup
# records tab and worktree switches together), so the rollup cannot separate them.
#
# Usage: flood.sh [--bin PATH] [--mode seq|cat-file|many-panes] [--panes N]
#                 [--worktrees N] [--floods N] [--switches N] [--tab-switches N]
#                 [--json]
# TG_PERF_MODE/TG_PERF_SETTLE_MS/TG_PERF_MARKERS/TG_PERF_SPLIT_MS/
# TG_PERF_CAT_BYTES tune defaults. TG_PERF_SANDBOX=1 opts into measuring
# sandboxed panes.
#
# Exit status: 0 ok (numbers printed, correctness checks passed); 1 harness
# error or a failed correctness check (budget breaches still exit 0).

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=test/perf/lib/env.sh disable=SC1091
source "$HERE/lib/env.sh"
# shellcheck source=test/perf/lib/fixture.sh disable=SC1091
source "$HERE/lib/fixture.sh"
# shellcheck source=test/lib/pty.sh disable=SC1091
source "$HERE/../lib/pty.sh"

BIN="${TG_PERF_BIN:-target/release/thegn}"
WORKTREES="${TG_PERF_WORKTREES:-8}"
MODE="${TG_PERF_MODE:-seq}"
PANES=16
PANES_GIVEN=0
FLOODS=3        # how many panes/shells run a flood
SWITCHES=30     # Alt+Down burst size, fired mid-flood
TAB_SWITCHES=30 # Alt+Right burst size, fired after the worktree burst
JSON_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
  --bin)
    BIN="$2"
    shift 2
    ;;
  --mode)
    MODE="$2"
    shift 2
    ;;
  --panes)
    PANES="$2"
    PANES_GIVEN=1
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
  --tab-switches)
    TAB_SWITCHES="$2"
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

case "$MODE" in
seq | cat-file | many-panes) ;;
*)
  echo "flood: unknown mode: $MODE (want seq|cat-file|many-panes)" >&2
  exit 1
  ;;
esac
if [ "$PANES_GIVEN" = 1 ] && [ "$MODE" != many-panes ]; then
  echo "flood: --panes requires --mode many-panes" >&2
  exit 1
fi
case "${PANES}" in
'' | *[!0-9]*)
  echo "flood: --panes must be a number, got: $PANES" >&2
  exit 1
  ;;
esac
if [ "$MODE" != many-panes ] && [ "$WORKTREES" -le "$FLOODS" ]; then
  echo "flood: need WORKTREES > FLOODS so the burst lands on a quiet pane" >&2
  exit 1
fi
if [ "$MODE" = many-panes ]; then
  if [ "$PANES" -lt $((FLOODS + 2)) ]; then
    echo "flood: --panes must be >= FLOODS+2 so a quiet pane exists beyond the flooders" >&2
    exit 1
  fi
  if [ "$WORKTREES" -lt 2 ]; then
    echo "flood: many-panes still needs >=2 worktrees for the switch burst" >&2
    exit 1
  fi
fi

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
perf_sandbox_off_default
perf_trap_cleanup
HOST_TAG="$(perf_host_tag)"
REPO="$(perf_build_fixture "$WORKTREES" 0)"

command -v script >/dev/null 2>&1 || {
  echo "flood: script(1) not found" >&2
  exit 1
}

# Heavy-output fixture: a big file with repeating content for `cat` to dump.
# `yes | head -c` is portable (POSIX yes + -c on BSD/GNU head); the `|| true`
# is not cosmetic — `head -c` closes the pipe and `yes` dies with SIGPIPE,
# which pipefail would otherwise treat as a harness error. Read it back once
# to warm the page cache: the flood should measure the pty path, not a cold
# disk. (`BIG_BYTES` is re-read from the file, so a truncated read is visible.)
BIG=""
BIG_BYTES=0
if [ "$MODE" != seq ]; then
  BIG="$PERF_TMP/flood-cat.bin"
  BIG_BYTES="${TG_PERF_CAT_BYTES:-4194304}"
  yes '0123456789abcdefghij' | head -c "$BIG_BYTES" >"$BIG" || true
  cat "$BIG" >/dev/null
  BIG_BYTES="$(wc -c <"$BIG" | tr -d ' ')"
fi
case "$MODE" in
seq) FLOOD_KEYS='seq 1 100000000\r' ;;
*) FLOOD_KEYS="while :; do cat '$BIG'; done\r" ;;
esac

PIDFILE="$PERF_TMP/thegn.pid"
TSCRIPT="$PERF_TMP/flood-typescript"
# settle + wizard dismiss + flood/build + markers + burst + rollup tail. The
# perf interval is forced low so several rollups land inside the window.
# Big-fixture runs need a longer settle (startup hydration of N worktrees),
# or the first typed keys land mid-storm and pollute the input percentiles.
SETTLE_MS="${TG_PERF_SETTLE_MS:-3000}"
ESC_MS=1100 # Esc + 0.5 + Esc + 0.5: dismiss the first-launch wizard
MARKERS="${TG_PERF_MARKERS:-6}"
MARKER_GAP_MS=500
SPLIT_MS="${TG_PERF_SPLIT_MS:-600}" # settle after each Alt+p/Alt+t key
PANES_PER_TAB=16
BURST_GAP_MS=120
# + pre-marker settle, the pre-marker itself, and M markers
MARKER_MS=$(((MARKERS + 1) * MARKER_GAP_MS + 500))
if [ "$MODE" = many-panes ]; then
  # (PANES−1) keys grow the layout — every pane after the first costs one
  # Alt+p split, and every tab after the first costs one Alt+t instead — so
  # splits + tab creates = PANES−1, each followed by a settle that also
  # covers typing the flood command; +2000 lets the last-started floods ramp
  # before the markers begin.
  BUILD_MS=$(((PANES - 1) * SPLIT_MS + 2000))
  FLOOD_PHASE_MS=0
else
  BUILD_MS=0
  FLOOD_PHASE_MS=$((FLOODS * 800))
fi
RUN_MS=$((SETTLE_MS + ESC_MS + BUILD_MS + FLOOD_PHASE_MS + MARKER_MS + (SWITCHES + TAB_SWITCHES) * BURST_GAP_MS + 4000))
DEADLINE_S=$(((RUN_MS / 1000) + 15))
# Precomputed second-valued sleeps: the key loops below fire up to ~64 of them,
# and a fresh awk per sleep would put harness noise inside the measurement.
# BEGIN-only awk never reads stdin (same idiom as the settle sleep below).
SPLIT_S="$(awk "BEGIN{print $SPLIT_MS/1000}")"
MARKER_GAP_S="$(awk "BEGIN{print $MARKER_GAP_MS/1000}")"
BURST_GAP_S="$(awk "BEGIN{print $BURST_GAP_MS/1000}")"

printf -v INNER \
  'cd %q; stty rows 50 cols 200; env THEGN_BENCH_RUN_MS=%q THEGN_PERF=1 THEGN_PERF_INTERVAL_MS=2000 THEGN_LOG=thegn::perf=debug %q & echo $! > %q; wait' \
  "$REPO" "$RUN_MS" "$BIN_ABS" "$PIDFILE"
# Key injection goes through script(1)'s STDIN (it forwards to the pty
# MASTER). Writing to thegn's /proc fd/0 would hit the pty *slave* — that's
# pane output, not input. A held-open FIFO keeps script from seeing EOF.
FIFO="$PERF_TMP/keys.fifo"
mkfifo "$FIFO"
# `script`/`timeout` are both non-portable (util-linux vs BSD, GNU coreutils
# vs absent) — test/lib/pty.sh resolves both.
TIMEOUT="$(pty_timeout_bin)"
[ -n "$TIMEOUT" ] || {
  echo "flood: no timeout(1)/gtimeout(1) — install coreutils" >&2
  exit 1
}
# Single quotes are deliberate: $0/$1/$2 are the INNER bash's positionals,
# bound by the three arguments below, not this shell's. $1=INNER, $2=TSCRIPT.
# shellcheck disable=SC2016
"$TIMEOUT" "${DEADLINE_S}s" bash -c 'source "$0"; pty_run_to "$2" "$1"' \
  "$HERE/../lib/pty.sh" "$INNER" "$TSCRIPT" <"$FIFO" >/dev/null 2>&1 &
LAUNCHER=$!
exec 3>"$FIFO"

for _ in $(seq 1 100); do
  [ -s "$PIDFILE" ] && break
  sleep 0.05
done
PID="$(cat "$PIDFILE" 2>/dev/null || true)"
# `kill -0`, not `[ -d /proc/$PID ]`: this harness reads everything else from
# the `thegn::perf` rollup in the log, which is platform-independent, so the
# liveness probe was the ONLY thing pinning it to Linux — and it failed on macOS
# with a misleading "thegn did not start" rather than skipping.
if [ -z "$PID" ] || ! kill -0 "$PID" 2>/dev/null; then
  echo "flood: thegn did not start" >&2
  kill "$LAUNCHER" 2>/dev/null || true
  exit 1
fi

keys() { # write raw bytes to the outer pty master via script's stdin
  printf '%b' "$1" >&3 2>/dev/null || true
}

sleep "$(awk "BEGIN{print $SETTLE_MS/1000}")"

# Dismiss the first-launch setup wizard (fresh XDG state every run) so the
# burst keys reach the compositor's keymap, not a modal. Esc is "later" —
# Enter would ADVANCE the wizard, whose path inputs then eat the burst.
# Twice with a gap (t3-workspace-switch.sh's idiom): the first may land
# before the wizard paints.
keys '\x1b'
sleep 0.5
keys '\x1b'
sleep 0.5

if [ "$MODE" = many-panes ]; then
  # Build the pane sweep in the ACTIVE worktree. Alt+p smart-splits the
  # focused pane and focuses the new one, so the flood command typed after
  # the settle lands in the pane just created; extra panes beyond the host's
  # per-tab cap spill into Alt+t tabs. Flooders are the first $FLOODS splits;
  # the last-created pane never floods — that is the quiet pane, and focus
  # ends up there. (If the host's per-tab cap changes, the extra Alt+p here
  # hits the "Pane limit reached" status and types the flood into an existing
  # pane, so the run fails its own correctness check loudly.)
  remaining="$PANES"
  built=0
  while [ "$remaining" -gt 0 ]; do
    if [ "$built" -gt 0 ]; then
      keys '\x1bt' # Alt+t → new tab in the same worktree, fresh pane focused
      sleep "$SPLIT_S"
    fi
    count=$((remaining < PANES_PER_TAB ? remaining : PANES_PER_TAB))
    for _ in $(seq 1 $((count - 1))); do
      keys '\x1bp' # Alt+p → smart split; focus lands on the new pane
      sleep "$SPLIT_S"
      built=$((built + 1))
      if [ "$built" -le "$FLOODS" ]; then
        keys "$FLOOD_KEYS"
      fi
    done
    remaining=$((remaining - count))
  done
else
  # Start a flood in the focused shells of $FLOODS worktrees: type the
  # command, Enter, then Alt+Down to the next worktree. The flood keeps
  # producing in the background after we switch away. The LAST Alt+Down
  # lands on a worktree that never floods — the quiet pane.
  for _ in $(seq 1 "$FLOODS"); do
    keys "$FLOOD_KEYS"
    sleep 0.4
    keys '\x1b[1;3B' # Alt+Down → nav-down (focus down; next worktree at the edge)
    sleep 0.4
  done
fi

# Mid-flood quiet-pane markers: every command appends one byte-exact line to
# $HOME/quiet.log (the pane shell inherits the harness's isolated HOME) and
# echoes the same marker into the pane, which the typescript must show. The
# pre-marker (TG-QUIET-0) makes the quiet pane non-empty BEFORE the measured
# markers, so a truncating `>` cannot erase a silently-short history, and it
# doubles as the "quiet shell is up" canary — a too-slow landing materialize
# fails the run with 0 markers instead of a mystery. No `grep -q` anywhere
# near this file: it exits at the first match, the upstream pipe dies of
# SIGPIPE, and pipefail turns a PASS into a fail.
sleep 0.5
keys "echo \"TG-QUIET-0\">>\"\$HOME/quiet.log\"; echo TG-QUIET-0\r"
sleep "$MARKER_GAP_S"
for i in $(seq 1 "$MARKERS"); do
  keys "echo \"TG-QUIET-$i\">>\"\$HOME/quiet.log\"; echo TG-QUIET-$i\r"
  sleep "$MARKER_GAP_S"
done

# Mid-flood switch burst: the measurement the harness exists for. With a grid
# built, Alt+Down resolves pane-neighbours first, so the first presses may
# move focus inside the grid before escaping to the next worktree — the hops
# are bounded by the grid depth, move focus only (nothing is typed), and the
# rest of the burst is worktree switches like the other modes.
for _ in $(seq 1 "$SWITCHES"); do
  keys '\x1b[1;3B'
  sleep "$BURST_GAP_S"
done

# Mid-flood tab burst: Alt+Right = nav-right (next tab), same gap as above.
for _ in $(seq 1 "$TAB_SWITCHES"); do
  keys '\x1b[1;3C'
  sleep "$(awk "BEGIN{print $BURST_GAP_MS/1000}")"
done

exec 3>&- # close the key channel; thegn exits on its bench window
wait "$LAUNCHER" 2>/dev/null || true

LOG="$XDG_STATE_HOME/thegn/logs/thegn.log"
[ -f "$LOG" ] || {
  echo "flood: no thegn log at $LOG" >&2
  exit 1
}

# Pull every rollup's fields. The file sink writes plain `field=value` pairs
# (log_trace.rs installs Brand { ansi: false } for files); the SGR strip is
# belt-and-braces in case another styling layer lands. Never fail the
# pipeline on a missing field (pipefail + set -e). The `(^| )` anchor is
# load-bearing: `input_p99_us=` unanchored also matches inside
# `writer_input_p99_us=`, reporting the writer's completion tail instead of
# the input→frame latency the harness is for.
extract_all() { # $1 = field name -> every rollup's value, in log order
  { sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -oE "(^| )${1}=[0-9.]+" | cut -d= -f2; } || true
}
extract() { # $1 = field name -> max value across rollups (the worst interval)
  extract_all "$1" | sort -rn | head -1
}
extract_p99() { # $1 = field name -> nearest-rank p99 across all rollups
  extract_all "$1" | sort -n | awk '{ v[NR] = $1 } END { if (NR > 0) printf "%s", v[int((NR * 99 + 99) / 100)] }'
}
sum_field() { # $1 = field name -> total across rollups
  extract_all "$1" | awk '{ t += $1 } END { printf "%d", t + 0 }'
}
ROLLUPS="$(grep -c 'perf rollup' "$LOG" || true)"
[ "${ROLLUPS:-0}" -gt 0 ] || {
  echo "flood: no perf rollups captured (run too short?)" >&2
  exit 1
}
SWITCH_P50="$(extract switch_p50_us)"
SWITCH_P99="$(extract switch_p99_us)"
SWITCH_P99_ALL="$(extract_p99 switch_p99_us)"
INPUT_P50="$(extract input_p50_us)"
INPUT_P99="$(extract input_p99_us)"
INPUT_P99_ALL="$(extract_p99 input_p99_us)"
INPUT_SAMPLES_TOTAL="$(sum_field input_samples)"
DRAIN_P99="$(extract drain_p99_us)"
FLUSH_P99="$(extract flush_p99_us)"
RENDER_P99="$(extract render_p99_us)"
PTY_BPS="$(extract pty_bytes_per_s)"
# Bytes the host accounted across the whole run: Σ pty_bytes_per_s ×
# interval_seconds over every rollup. A floor, not an exact count — the final
# partial interval may never be logged — which is why the cat flood's
# producer is reported, not gated.
PTY_BYTES_TOTAL="$({
  sed 's/\x1b\[[0-9;]*m//g' "$LOG" | awk '/perf rollup/ {
    b = 0; s = 0
    for (i = 1; i <= NF; i++) {
      if ($i ~ /^pty_bytes_per_s=/) { v = $i; sub(/^pty_bytes_per_s=/, "", v); b = v + 0 }
      if ($i ~ /^interval_seconds=/) { v = $i; sub(/^interval_seconds=/, "", v); s = v + 0 }
    }
    t += b * s
  } END { printf "%.0f", t + 0 }'
} || true)"

# --- Correctness: quiet-pane markers -----------------------------------------
QUIET_LOG="$HOME/quiet.log"
QUIET_EXPECTED="$PERF_TMP/quiet.expected"
: >"$QUIET_EXPECTED"
printf 'TG-QUIET-0\n' >>"$QUIET_EXPECTED" # the pre-marker
for i in $(seq 1 "$MARKERS"); do
  printf 'TG-QUIET-%s\n' "$i" >>"$QUIET_EXPECTED"
done
QUIET_LINES=0
if [ -f "$QUIET_LOG" ]; then
  QUIET_LINES="$(grep -c '^TG-QUIET-' "$QUIET_LOG" || true)"
fi
QUIET_TOTAL=$((MARKERS + 1)) # markers plus the pre-marker
QUIET_OK=0
if [ -f "$QUIET_LOG" ] && cmp -s "$QUIET_LOG" "$QUIET_EXPECTED"; then
  QUIET_OK=1
fi
# The echo marker must reach the rendered output stream: the quiet pane's
# bytes still arrive under flood. The marker text is plain cells, so grep the
# typescript directly — no strip and no `grep -q` (it exits at the first
# match, the upstream pipe dies of SIGPIPE, and pipefail turns a PASS into
# a fail).
ECHO_OK=0
if [ -f "$TSCRIPT" ] && grep -a 'TG-QUIET-' "$TSCRIPT" >/dev/null; then
  ECHO_OK=1
fi

# Proposed budgets (docs/performance-benchmark-protocol.md): advisory gates on
# the pooled p99, not per-rollup maxima — one noisy window should not fail a
# good run, and the worst is printed right next to it.
INPUT_BUDGET_US=50000
SWITCH_BUDGET_US=250000
BUDGET_OK=1
[ "${INPUT_P99_ALL:-0}" -le "$INPUT_BUDGET_US" ] || BUDGET_OK=0
[ "${SWITCH_P99_ALL:-0}" -le "$SWITCH_BUDGET_US" ] || BUDGET_OK=0

PANES_JSON=null
DESC="mode=$MODE"
if [ "$MODE" = many-panes ]; then
  PANES_JSON=$PANES
  DESC="$DESC panes=$PANES"
fi

RESULT="{\"scenario\":\"flood\",\"mode\":\"$MODE\",\"build\":\"$BUILD\",\"panes\":$PANES_JSON,\"worktrees\":$WORKTREES,\"floods\":$FLOODS,\"switches\":$SWITCHES,\"tab_switches\":$TAB_SWITCHES,\"cat_bytes\":$BIG_BYTES,\"quiet_markers\":$MARKERS,\"switch_p50_us\":${SWITCH_P50:-0},\"switch_p99_us\":${SWITCH_P99:-0},\"switch_p99_all_us\":${SWITCH_P99_ALL:-0},\"input_p50_us\":${INPUT_P50:-0},\"input_p99_us\":${INPUT_P99:-0},\"input_p99_all_us\":${INPUT_P99_ALL:-0},\"input_samples_total\":${INPUT_SAMPLES_TOTAL:-0},\"drain_p99_us\":${DRAIN_P99:-0},\"flush_p99_us\":${FLUSH_P99:-0},\"render_p99_us\":${RENDER_P99:-0},\"pty_bytes_per_s\":${PTY_BPS:-0},\"pty_bytes_total\":${PTY_BYTES_TOTAL:-0},\"quiet_ok\":$QUIET_OK,\"echo_ok\":$ECHO_OK,\"budget_ok\":$BUDGET_OK,\"input_budget_us\":$INPUT_BUDGET_US,\"switch_budget_us\":$SWITCH_BUDGET_US,\"rollups\":$ROLLUPS,\"git_sha\":\"$GIT_SHA\",\"host_tag\":\"$HOST_TAG\"}"

if [ "$JSON_ONLY" = 1 ]; then
  printf '%s\n' "$RESULT"
else
  echo "scenario=flood $DESC build=$BUILD worktrees=$WORKTREES floods=$FLOODS switches=$SWITCHES tab_switches=$TAB_SWITCHES (sha=$GIT_SHA host=$HOST_TAG)"
  echo "  switch→frame (worktree+tab)  p50=${SWITCH_P50:-–}us  p99=${SWITCH_P99:-–}us (worst)  p99=${SWITCH_P99_ALL:-–}us (all rollups)"
  echo "  input→frame   p50=${INPUT_P50:-–}us  p99=${INPUT_P99:-–}us (worst)  p99=${INPUT_P99_ALL:-–}us (all rollups)  samples=${INPUT_SAMPLES_TOTAL:-0}"
  echo "  drain p99=${DRAIN_P99:-–}us  flush p99=${FLUSH_P99:-–}us  render p99=${RENDER_P99:-–}us"
  echo "  pty_bytes_per_s=${PTY_BPS:-0} (worst)  accounted=${PTY_BYTES_TOTAL:-0}B total  cat_bytes/flooder=$BIG_BYTES  rollups=$ROLLUPS"
  if [ "$QUIET_OK" = 1 ]; then
    echo "  quiet pane: $QUIET_LINES/$QUIET_TOTAL lines, byte-exact, in order"
  else
    echo "  quiet pane: $QUIET_LINES/$QUIET_TOTAL lines (want byte-exact, in order)"
  fi
  if [ "$ECHO_OK" = 1 ]; then
    echo "  quiet-pane echo reached the rendered stream: yes"
  else
    echo "  quiet-pane echo reached the rendered stream: NO"
  fi
  if [ "$BUDGET_OK" = 1 ]; then
    echo "  budgets: within proposed input ${INPUT_BUDGET_US}us / switch ${SWITCH_BUDGET_US}us (pooled p99)"
  else
    echo "  budgets: OVER proposed (input ${INPUT_P99_ALL:-0}us > ${INPUT_BUDGET_US} or switch ${SWITCH_P99_ALL:-0}us > ${SWITCH_BUDGET_US}) — file a perf bug and soften the claim"
  fi
fi

if [ "$QUIET_OK" != 1 ] || [ "$ECHO_OK" != 1 ]; then
  echo "flood: correctness checks FAILED (dropped keys / ordering / quiet-pane echo)" >&2
  exit 1
fi
