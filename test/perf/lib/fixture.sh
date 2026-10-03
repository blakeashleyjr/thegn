#!/usr/bin/env bash
# Build a hermetic fixture repo with N git worktrees for the perf harness.
#
# Reproduces the shape the idle-CPU regression was found in: a repo with many
# worktrees, each of which the 2s model-refresh ticker fans a git scan over.
# The Rust micro-bench fixture (crates/thegn-svc/benches/support/fixture.rs)
# builds the same layout — keep the two in sync.
#
# Requires the isolated env from env.sh to already be sourced + perf_make_tmp run
# (so HOME / GIT_CONFIG_GLOBAL are sandboxed). Echoes the primary worktree path.

set -euo pipefail

# perf_build_fixture <num_worktrees> <num_dirty>
# Creates $PERF_TMP/repo (with an `origin` bare remote so ahead/behind has an
# upstream) plus <num_worktrees> linked worktrees under $PERF_TMP/worktrees/.
# Marks the first <num_dirty> worktrees dirty (an untracked file) so is_dirty
# does real work. Echoes the primary worktree path (cwd for the harness).
perf_build_fixture() {
  local n="${1:-14}" dirty="${2:-4}"
  local root="$PERF_TMP/repo" origin="$PERF_TMP/origin.git"

  git init -q -b main "$root"
  (
    cd "$root"
    # A handful of files so a git-status scan has a tree to walk.
    for i in $(seq 1 20); do printf 'line %s\n' "$i" >"file_$i.txt"; done
    mkdir -p src
    for i in $(seq 1 20); do printf 'fn f%s() {}\n' "$i" >"src/mod_$i.rs"; done
    git add -A
    git -c commit.gpgsign=false commit -q -m "seed"
  )

  # Bare origin so `ahead_behind` resolves an upstream.
  git clone -q --bare "$root" "$origin"
  (
    cd "$root"
    git remote add origin "$origin"
    git fetch -q origin
    git branch -q --set-upstream-to=origin/main main || true
  )

  mkdir -p "$PERF_TMP/worktrees"
  for i in $(seq 1 "$n"); do
    git -C "$root" worktree add -q -b "wt-$i" "$PERF_TMP/worktrees/wt-$i" main
    # Track origin/main so ahead/behind does real work (as production worktrees do).
    git -C "$PERF_TMP/worktrees/wt-$i" branch -q --set-upstream-to=origin/main "wt-$i" 2>/dev/null || true
  done

  # Dirty the first <dirty> worktrees.
  for i in $(seq 1 "$dirty"); do
    [ "$i" -le "$n" ] || break
    printf 'scratch\n' >"$PERF_TMP/worktrees/wt-$i/UNCOMMITTED.txt"
  done

  # Pack the refs now. thegn's startup auto-fetch runs git's auto-maintenance,
  # which packs 40+ loose refs on the first fetch: packed-refs is rewritten and
  # every loose ref unlinked a few seconds into the session. That is a real
  # (storage) change the change-generation correctly reports, and its consequences
  # (heal / branch re-read / commit refresh) then trail into the idle window --
  # startup transient, not idle cost. A fixture that models a repo whose refs are
  # already packed (gc'd) keeps the window an idle window.
  git -C "$root" pack-refs --all

  echo "$root"
}
