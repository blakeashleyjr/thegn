#!/usr/bin/env bash
# Workspace-switch (T3) latency: the heaviest switch tier.
#
# flood.sh measures worktree switches (T2) inside one workspace; nothing there
# exercises `switch_workspace` — the path that parks the session, persists the
# layout, and (cold) resurrects the target. This scenario builds several
# fixture REPOS, registers each as a workspace (a short thegn launch in each —
# the first hydration `put_workspace`s it, the production registration path),
# then runs thegn in the first repo and fires a Shift+Alt+Down burst around
# the workspace ring, reading switch_ws_p50/p99 from the perf rollup.
#
# Advisory (machine-dependent) — NOT a CI gate. Capture before/after evidence
# for workspace-switch changes: the numbers to watch are switch_ws_p99_us and
# render_full_p99_us.
#
# Usage: t3-workspace-switch.sh [--bin PATH] [--workspaces N] [--worktrees N]
#                               [--switches N] [--json]
#
# Exit status: 0 ok (numbers printed); 1 harness error.

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=test/perf/lib/env.sh disable=SC1091
source "$HERE/lib/env.sh"
# shellcheck source=test/lib/pty.sh disable=SC1091
source "$HERE/../lib/pty.sh"

BIN="${TG_PERF_BIN:-target/release/thegn}"
WORKSPACES="${TG_PERF_WORKSPACES:-3}"
WORKTREES="${TG_PERF_WORKTREES:-3}" # per workspace
SWITCHES=20                         # Shift+Alt+Down burst size
JSON_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
  --bin)
    BIN="$2"
    shift 2
    ;;
  --workspaces)
    WORKSPACES="$2"
    shift 2
    ;;
  --worktrees)
    WORKTREES="$2"
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
    echo "t3: unknown arg: $1" >&2
    exit 1
    ;;
  esac
done

BIN_ABS="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
[ -x "$BIN_ABS" ] || {
  echo "t3: binary not executable: $BIN_ABS" >&2
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

command -v script >/dev/null 2>&1 || {
  echo "t3: script(1) not found" >&2
  exit 1
}
TIMEOUT="$(pty_timeout_bin)"
[ -n "$TIMEOUT" ] || {
  echo "t3: no timeout(1)/gtimeout(1) — install coreutils" >&2
  exit 1
}

# t3_build_repo <name> <num_worktrees> — a seeded repo + N linked worktrees
# (same shape as lib/fixture.sh's perf_build_fixture, parameterized by name so
# several REPOS coexist under $PERF_TMP). Echoes the repo path.
t3_build_repo() {
  local name="$1" n="$2"
  local root="$PERF_TMP/$name"
  git init -q -b main "$root"
  (
    cd "$root"
    for i in $(seq 1 20); do printf 'line %s\n' "$i" >"file_$i.txt"; done
    git add -A
    git -c commit.gpgsign=false commit -q -m "seed"
  )
  mkdir -p "$PERF_TMP/$name-worktrees"
  for i in $(seq 1 "$n"); do
    git -C "$root" worktree add -q -b "wt-$i" "$PERF_TMP/$name-worktrees/wt-$i" main
  done
  echo "$root"
}

REPOS=()
for w in $(seq 1 "$WORKSPACES"); do
  REPOS+=("$(t3_build_repo "repo-$w" "$WORKTREES")")
done

# Register every repo as a workspace: point the launch at the repo with
# `thegn open --no-launch` (a bare launch resumes the LAST-ACTIVE workspace,
# not the cwd repo), then a short headless launch whose first hydration
# `put_workspace`s it into the shared DB (hydrate.rs).
REG_MS=2500
for repo in "${REPOS[@]}"; do
  "$BIN_ABS" open "$repo" --no-launch >/dev/null 2>&1 || true
  printf -v REG_INNER 'cd %q; stty rows 50 cols 200; env THEGN_BENCH_RUN_MS=%q %q' \
    "$repo" "$REG_MS" "$BIN_ABS"
  # shellcheck disable=SC2016
  "$TIMEOUT" 30s bash -c 'source "$0"; pty_run "$1"' \
    "$HERE/../lib/pty.sh" "$REG_INNER" </dev/null >/dev/null 2>&1 || true
done
# The main run must land in the FIRST repo, not whichever registered last.
"$BIN_ABS" open "${REPOS[0]}" --no-launch >/dev/null 2>&1 || true

# Main run: launch in the FIRST repo, settle, then the ring burst.
PIDFILE="$PERF_TMP/thegn.pid"
SETTLE_MS="${TG_PERF_SETTLE_MS:-5000}"
BURST_GAP_MS=250 # workspace switches are heavier than worktree hops
RUN_MS=$((SETTLE_MS + SWITCHES * BURST_GAP_MS + 5000))
DEADLINE_S=$(((RUN_MS / 1000) + 15))

printf -v INNER \
  'cd %q; stty rows 50 cols 200; env THEGN_BENCH_RUN_MS=%q THEGN_PERF=1 THEGN_PERF_INTERVAL_MS=2000 THEGN_LOG=thegn::perf=debug %q & echo $! > %q; wait' \
  "${REPOS[0]}" "$RUN_MS" "$BIN_ABS" "$PIDFILE"
FIFO="$PERF_TMP/keys.fifo"
mkfifo "$FIFO"
# shellcheck disable=SC2016
"$TIMEOUT" "${DEADLINE_S}s" bash -c 'source "$0"; pty_run "$1"' \
  "$HERE/../lib/pty.sh" "$INNER" <"$FIFO" >/dev/null 2>&1 &
LAUNCHER=$!
exec 3>"$FIFO"

for _ in $(seq 1 100); do
  [ -s "$PIDFILE" ] && break
  sleep 0.05
done
PID="$(cat "$PIDFILE" 2>/dev/null || true)"
[ -n "$PID" ] && [ -d "/proc/$PID" ] || {
  echo "t3: thegn did not start" >&2
  kill "$LAUNCHER" 2>/dev/null || true
  exit 1
}

keys() { printf '%b' "$1" >&3 2>/dev/null || true; }

sleep "$(awk "BEGIN{print $SETTLE_MS/1000}")"

# Dismiss the first-launch setup wizard (fresh XDG state every run): Esc is
# "later" — Enter would ADVANCE it to the next step and the modal would then
# swallow the whole burst. Sent twice with a gap in case the first lands
# before the wizard paints (a spare Esc in normal mode is a harmless
# escape-to-center).
keys '\x1b'
sleep 0.8
keys '\x1b'
sleep 0.8

# The workspace-ring burst: Shift+Alt+Down (CSI 1;4B) walks Next-Workspace.
for _ in $(seq 1 "$SWITCHES"); do
  keys '\x1b[1;4B'
  sleep "$(awk "BEGIN{print $BURST_GAP_MS/1000}")"
done

exec 3>&- # close the key channel; thegn exits on its bench window
wait "$LAUNCHER" 2>/dev/null || true

LOG="$XDG_STATE_HOME/thegn/logs/thegn.log"
[ -f "$LOG" ] || {
  echo "t3: no thegn log at $LOG" >&2
  exit 1
}

# Worst interval across rollups (same rationale as flood.sh).
extract() { # $1 = field name -> max value across rollups
  { sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -o "${1}=[0-9.]*" | cut -d= -f2 | sort -rn | head -1; } || true
}
ROLLUPS="$(grep -c 'perf rollup' "$LOG" || true)"
[ "${ROLLUPS:-0}" -gt 0 ] || {
  echo "t3: no perf rollups captured (run too short?)" >&2
  exit 1
}
WS_P50="$(extract switch_ws_p50_us)"
WS_P99="$(extract switch_ws_p99_us)"
INPUT_P99="$(extract input_p99_us)"
FULL_P50="$(extract render_full_p50_us)"
FULL_P99="$(extract render_full_p99_us)"
FLUSH_P99="$(extract flush_p99_us)"

# A burst that never landed a workspace switch (dialog ate the keys, ring had
# one stop, …) reads as 0 — call that out as a harness failure, not a win.
[ -n "${WS_P50:-}" ] && [ "${WS_P50:-0}" != "0" ] || {
  echo "t3: no workspace switches recorded — burst never reached the ring?" >&2
  exit 1
}

RESULT="{\"scenario\":\"t3-workspace-switch\",\"build\":\"$BUILD\",\"workspaces\":$WORKSPACES,\"worktrees\":$WORKTREES,\"switches\":$SWITCHES,\"switch_ws_p50_us\":${WS_P50:-0},\"switch_ws_p99_us\":${WS_P99:-0},\"input_p99_us\":${INPUT_P99:-0},\"render_full_p50_us\":${FULL_P50:-0},\"render_full_p99_us\":${FULL_P99:-0},\"flush_p99_us\":${FLUSH_P99:-0},\"rollups\":$ROLLUPS,\"git_sha\":\"$GIT_SHA\",\"host_tag\":\"$HOST_TAG\"}"

if [ "$JSON_ONLY" = 1 ]; then
  printf '%s\n' "$RESULT"
else
  echo "scenario=t3-workspace-switch build=$BUILD workspaces=$WORKSPACES worktrees=$WORKTREES switches=$SWITCHES (sha=$GIT_SHA host=$HOST_TAG)"
  echo "  workspace-switch→frame  p50=${WS_P50:-–}us  p99=${WS_P99:-–}us   (worst rollup)"
  echo "  full-frame render       p50=${FULL_P50:-–}us  p99=${FULL_P99:-–}us"
  echo "  input p99=${INPUT_P99:-–}us  flush p99=${FLUSH_P99:-–}us  rollups=$ROLLUPS"
fi
