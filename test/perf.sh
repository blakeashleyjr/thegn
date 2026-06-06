#!/usr/bin/env bash
# test/perf.sh — perf regression check for the sidebar's data feeds, run against
# a generated stress instance (see test/gen-fixture.sh). The sidebar re-pulls
# `superzej workspaces` on every tab change, so it MUST stay ~constant in the
# repo count — that's the fix this guards. `worktrees`/`list` are reported too
# (they do O(worktrees) git calls by necessity, so their budgets are loose).
#
# Usage: test/perf.sh [NAME]   (NAME = instance suffix, default: stress)
set -euo pipefail

NAME="${1:-stress}"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SZ="${SZ:-$ROOT_DIR/target/release/superzej}"
SZ="$(cd "$(dirname "$SZ")" && pwd)/$(basename "$SZ")"
INST="$HOME/.superzej-$NAME"

[[ -x $SZ ]] || {
  echo "not executable: $SZ (run: cargo build --release)" >&2
  exit 1
}
[[ -d $INST/state ]] || {
  echo "no instance at $INST — run: just stress-gen $NAME" >&2
  exit 1
}

export SUPERZEJ_DIR="$INST"
export XDG_STATE_HOME="$INST/state"
export XDG_CONFIG_HOME="$INST/config"
unset ZELLIJ ZELLIJ_SESSION_NAME ZELLIJ_PANE_ID

repos="$("$SZ" workspaces | grep -c .)"
wts="$("$SZ" worktrees | grep -c .)"
echo "instance: $INST  ($repos repos, $wts worktrees)"

fail=0
# median wall-clock (ms) of `superzej <args...>` over 5 runs.
bench() {
  local label="$1" budget="$2"
  shift 2
  local times=() t0 t1
  for _ in 1 2 3 4 5; do
    t0=$(date +%s%N)
    "$SZ" "$@" >/dev/null 2>&1 || true
    t1=$(date +%s%N)
    times+=($(((t1 - t0) / 1000000)))
  done
  # median of 5
  local med
  med=$(printf '%s\n' "${times[@]}" | sort -n | sed -n 3p)
  if ((med <= budget)); then
    printf '  \033[32mok\033[0m   %-26s %4d ms  (budget %d)\n' "$label" "$med" "$budget"
  else
    printf '  \033[31mFAIL\033[0m %-26s %4d ms  (budget %d)\n' "$label" "$med" "$budget"
    fail=1
  fi
}

# workspaces is the hot path (re-pulled on every TabUpdate). With the per-repo
# git + Db::open removed it's a couple of queries — must stay well under 100ms
# even on a release build with cold caches.
bench "workspaces (sidebar feed)" 100 workspaces
# These necessarily shell out to git per worktree (status + ahead/behind), so
# they scale with the worktree count; budgets are generous.
bench "worktrees" 4000 worktrees
bench "list --json" 4000 list --json

if ((fail == 0)); then
  echo "perf: green"
else
  echo "perf: FAILED"
  exit 1
fi
