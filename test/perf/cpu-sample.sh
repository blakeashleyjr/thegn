#!/usr/bin/env bash
# Steady-state / idle CPU harness for thegn.
#
# Launches thegn inside a PTY (via script(1), like test/pty-smoke.sh) in a
# fully isolated environment with a fixture repo of N worktrees, lets it settle,
# then samples the process's CPU (utime+stime from /proc) over a fixed window
# and reports cores-used with a per-thread breakdown. This finally measures the
# steady-state cost the launch→first-frame `just bench` never sees.
#
# Usage:
#   cpu-sample.sh [--scenario idle|steady-workload] [--bin PATH]
#                 [--worktrees N] [--dirty N] [--settle-ms MS] [--window-ms MS]
#                 [--ceiling CORES] [--record] [--json] [--baseline-dir DIR]
#
# Exit status: 0 ok; 2 over the idle ceiling; 1 harness error.

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=test/perf/lib/env.sh disable=SC1091
source "$HERE/lib/env.sh"
# shellcheck source=test/perf/lib/fixture.sh disable=SC1091
source "$HERE/lib/fixture.sh"

SCENARIO=idle
BIN="${TG_PERF_BIN:-target/release/thegn}"
WORKTREES="${TG_PERF_WORKTREES:-14}"
DIRTY="${TG_PERF_DIRTY:-4}"
SETTLE_MS=2500
WINDOW_MS=8000 # long enough to average the dashboard's 4s sysinfo cadence
# cores; the encoded 0%-idle guard (FIXED, not baseline-derived). Observed idle
# on the 14-worktree fixture (release) is ~0.056 cores, dominated by the
# pre-warmed dashboard collector; a true event-loop spin regression would be
# 0.5-1.5 cores, far past this. Tighten once the dashboard poll is visibility-gated.
CEILING=0.12
RECORD=0
JSON_ONLY=0
BASELINE_DIR="$HERE/baselines"

while [ $# -gt 0 ]; do
  case "$1" in
  --scenario)
    SCENARIO="$2"
    shift 2
    ;;
  --bin)
    BIN="$2"
    shift 2
    ;;
  --worktrees)
    WORKTREES="$2"
    shift 2
    ;;
  --dirty)
    DIRTY="$2"
    shift 2
    ;;
  --settle-ms)
    SETTLE_MS="$2"
    shift 2
    ;;
  --window-ms)
    WINDOW_MS="$2"
    shift 2
    ;;
  --ceiling)
    CEILING="$2"
    shift 2
    ;;
  --record)
    RECORD=1
    shift
    ;;
  --json)
    JSON_ONLY=1
    shift
    ;;
  --baseline-dir)
    BASELINE_DIR="$2"
    shift 2
    ;;
  *)
    echo "cpu-sample: unknown arg: $1" >&2
    exit 1
    ;;
  esac
done

BIN_ABS="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
[ -x "$BIN_ABS" ] || {
  echo "cpu-sample: binary not executable: $BIN_ABS" >&2
  exit 1
}
case "$BIN_ABS" in
*target/release/*) BUILD=release ;;
*target/debug/*) BUILD=debug ;;
*) BUILD=unknown ;;
esac

CLK_TCK="$(getconf CLK_TCK 2>/dev/null || echo 100)"
GIT_SHA="$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo unknown)"

perf_make_tmp
perf_trap_cleanup
HOST_TAG="$(perf_host_tag)"
REPO="$(perf_build_fixture "$WORKTREES" "$DIRTY")"

command -v script >/dev/null 2>&1 || {
  echo "cpu-sample: script(1) not found" >&2
  exit 1
}

PIDFILE="$PERF_TMP/thegn.pid"
RUN_MS=$((SETTLE_MS + WINDOW_MS + 1500)) # generous tail past the sample window
DEADLINE_S=$(((RUN_MS / 1000) + 10))     # hard safety net

# Launch thegn in a PTY. `script -qec` gives termwiz a real terminal (mirrors
# test/pty-smoke.sh); the inner shell backgrounds thegn and records its PID so
# we can sample /proc directly. THEGN_BENCH_RUN_MS makes thegn run the full
# loop — ticker, hydration, tokio pool — then exit cleanly on its own.
# THEGN_NO_DAEMON: the bench-window exit detaches daemon panes — each run
# would strand a never-reaped session (and its daemon) in the bench state dir.
printf -v INNER \
  'cd %q; stty rows 50 cols 200; env THEGN_BENCH_RUN_MS=%q THEGN_NO_DAEMON=1 %q & echo $! > %q; wait' \
  "$REPO" "$RUN_MS" "$BIN_ABS" "$PIDFILE"
timeout "${DEADLINE_S}s" script -qec "$INNER" /dev/null >/dev/null 2>&1 &
LAUNCHER=$!

# Wait for the PID file (thegn up).
for _ in $(seq 1 100); do
  [ -s "$PIDFILE" ] && break
  sleep 0.05
done
PID="$(cat "$PIDFILE" 2>/dev/null || true)"
[ -n "$PID" ] && [ -d "/proc/$PID" ] || {
  echo "cpu-sample: thegn did not start" >&2
  kill "$LAUNCHER" 2>/dev/null || true
  exit 1
}

# /proc/<pid>/stat fields 14,15 = utime,stime (in CLK_TCK). The comm field (2)
# may contain spaces/parens, so split on the LAST ')'.
proc_jiffies() { # $1 = pid -> utime+stime
  awk '{ s=$0; sub(/^.*\) /,"",s); split(s,a," "); print a[12]+a[13] }' "/proc/$1/stat" 2>/dev/null || echo 0
}

if [ "$SCENARIO" = steady-workload ]; then
  KEYS="$HERE/scenarios/steady-workload.keys"
  [ -f "$KEYS" ] && cat "$KEYS" >"/proc/$PID/fd/0" 2>/dev/null || true
fi

# Settle, then capture the process + per-thread baseline at the SAME instant
# (window start), sleep the window, and diff.
sleep "$(awk "BEGIN{print $SETTLE_MS/1000}")"
J0="$(proc_jiffies "$PID")"
declare -A T0 TN
for tid_dir in "/proc/$PID/task"/*; do
  [ -e "$tid_dir" ] || continue # glob stays literal if the process vanished
  tid="${tid_dir##*/}"
  T0[$tid]="$(awk '{ s=$0; sub(/^.*\) /,"",s); split(s,a," "); print a[12]+a[13] }' "/proc/$PID/task/$tid/stat" 2>/dev/null || echo 0)"
  TN[$tid]="$(cat "/proc/$PID/task/$tid/comm" 2>/dev/null || echo '?')"
done
sleep "$(awk "BEGIN{print $WINDOW_MS/1000}")"
J1="$(proc_jiffies "$PID")"

WINDOW_S="$(awk "BEGIN{print $WINDOW_MS/1000}")"
CORES_TOTAL="$(awk "BEGIN{printf \"%.4f\", ($J1-$J0)/($CLK_TCK*$WINDOW_S)}")"

# Per-thread deltas. Capture a sorted "comm cores" table for display and a JSON
# array for the result. Read t1 BEFORE thegn exits (we're still inside the window
# tail). Done set -e-safe — a vanished tid just contributes nothing.
THREAD_JSON=""
THREAD_TABLE=""
for tid in "${!T0[@]}"; do
  t1="$(awk '{ s=$0; sub(/^.*\) /,"",s); split(s,a," "); print a[12]+a[13] }' "/proc/$PID/task/$tid/stat" 2>/dev/null || true)"
  [ -n "$t1" ] || t1="${T0[$tid]}"
  dj=$((t1 - ${T0[$tid]}))
  [ "$dj" -gt 0 ] || continue
  c="$(awk "BEGIN{printf \"%.4f\", $dj/($CLK_TCK*$WINDOW_S)}")"
  THREAD_JSON="$THREAD_JSON{\"tid\":$tid,\"comm\":\"${TN[$tid]}\",\"cores\":$c},"
  THREAD_TABLE="$THREAD_TABLE$c ${TN[$tid]} (tid $tid)"$'\n'
done
THREAD_JSON="[${THREAD_JSON%,}]"

# Let thegn exit on its own (bench window), then reap the launcher.
wait "$LAUNCHER" 2>/dev/null || true

RESULT="{\"scenario\":\"$SCENARIO\",\"build\":\"$BUILD\",\"worktrees\":$WORKTREES,\"window_ms\":$WINDOW_MS,\"cores_total\":$CORES_TOTAL,\"threads\":$THREAD_JSON,\"git_sha\":\"$GIT_SHA\",\"host_tag\":\"$HOST_TAG\"}"

BASELINE="$BASELINE_DIR/$HOST_TAG.$SCENARIO.json"
if [ "$RECORD" = 1 ]; then
  mkdir -p "$BASELINE_DIR"
  printf '%s\n' "$RESULT" >"$BASELINE"
fi

if [ "$JSON_ONLY" = 1 ]; then
  printf '%s\n' "$RESULT"
else
  echo "scenario=$SCENARIO build=$BUILD worktrees=$WORKTREES window=${WINDOW_MS}ms"
  echo "binary=$BIN_ABS"
  echo "cores_total=$CORES_TOTAL  (host=$HOST_TAG sha=$GIT_SHA)"
  echo "top threads (cores comm tid):"
  if [ -n "$THREAD_TABLE" ]; then
    printf '%s' "$THREAD_TABLE" | sort -rn | head -8 | sed 's/^/  /'
  else
    echo "  (no per-thread CPU captured)"
  fi
  if [ -f "$BASELINE" ]; then
    BASE_CORES="$(grep -o '"cores_total":[0-9.]*' "$BASELINE" | cut -d: -f2)"
    echo "baseline=$BASE_CORES  delta=$(awk "BEGIN{printf \"%+.4f\", $CORES_TOTAL-$BASE_CORES}")"
  fi
fi

# The idle scenario encodes the 0%-idle invariant against a FIXED ceiling so a
# regressed baseline can never silently raise the bar.
if [ "$SCENARIO" = idle ] && [ "$BUILD" = release ]; then
  if awk "BEGIN{exit !($CORES_TOTAL > $CEILING)}"; then
    echo "FAIL: idle cores_total=$CORES_TOTAL exceeds ceiling=$CEILING cores" >&2
    exit 2
  fi
fi
