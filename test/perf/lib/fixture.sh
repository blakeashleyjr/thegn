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

# perf_add_lfs_fixture <repo>
# Adds same-size stat-dirty LFS-shaped files to the primary worktree and installs
# a required clean+process filter backed by a canned protocol-v2 server. The
# config is repo-local because gix's handling of the isolated global config is
# not guaranteed to match Git's; linked worktrees share this .git/config.
perf_add_lfs_fixture() {
  local repo="$1" server_dir="$PERF_TMP/lfs-filter" server_bytes process_cmd
  server_bytes="$server_dir/server-bytes"
  mkdir -p "$server_dir"
  python3 - "$server_bytes" <<'PY'
import sys

def pkt(payload):
    return f"{len(payload) + 4:04x}".encode("ascii") + payload

greeting = pkt(b"git-filter-server\n") + pkt(b"version=2\n") + b"0000"
capabilities = pkt(b"capability=clean\n") + b"0000"
response = pkt(b"status=success\n") + b"0000" + pkt(b"lfs-clean\n") + b"0000" + pkt(b"status=success\n") + b"0000"
with open(sys.argv[1], "wb") as server:
    server.write(greeting)
    server.write(capabilities)
    server.write(response * 200000)
PY
  if [ "$(head -c 22 "$server_bytes")" != "0016git-filter-server" ]; then
    echo "FAIL: generated LFS filter server bytes have an invalid greeting: $server_bytes" >&2
    return 1
  fi

  printf '%s\n' '*.bin filter=lfs diff=lfs merge=lfs -text' >"$repo/.gitattributes"
  for i in 1 2 3; do printf 'blob-%s-v0\n' "$i" >"$repo/asset_$i.bin"; done
  git -C "$repo" add .gitattributes asset_1.bin asset_2.bin asset_3.bin
  git -C "$repo" -c commit.gpgsign=false commit -q -m "add LFS-shaped soak files"

  # shellcheck disable=SC2016 # The literal $1 is expanded by the configured child shell.
  printf -v process_cmd 'sh -c %q sh %q' 'cat "$1"; exec cat >/dev/null' "$server_bytes"
  git -C "$repo" config filter.lfs.process "$process_cmd"
  git -C "$repo" config filter.lfs.clean cat
  git -C "$repo" config filter.lfs.required true
  for i in 1 2 3; do
    printf 'blob-%s-v1\n' "$i" >"$repo/asset_$i.bin"
    touch "$repo/asset_$i.bin"
  done
  git -C "$repo" push -q origin main
}

# Run one isolated startup so the thegn DB and the workspace's own registry
# entry exist, then register each linked fixture worktree for session adoption.
# Requires the caller to have sourced test/lib/pty.sh and set $HERE.
perf_seed_fixture_registry() {
  local count="$1" bin="$2" root="$3" db inner timeout_bin
  db="$XDG_STATE_HOME/thegn/thegn.db"
  printf -v inner 'cd %q; stty rows 50 cols 200; env THEGN_NO_DAEMON=1 THEGN_BENCH_RUN_MS=3000 %q' \
    "$root" "$bin"
  timeout_bin="$(pty_timeout_bin)"
  if [ -n "$timeout_bin" ]; then
    # `$0`/`$1` are the bash -c positionals bound by the arguments below.
    # shellcheck disable=SC2016
    if ! "$timeout_bin" 30s bash -c 'source "$0"; pty_run "$1"' \
      "$HERE/../lib/pty.sh" "$inner" >/dev/null 2>&1; then
      echo 'FAIL: isolated registry warm-up launch failed or timed out' >&2
      return 1
    fi
  elif ! pty_run "$inner" >/dev/null 2>&1; then
    echo 'FAIL: isolated registry warm-up launch failed' >&2
    return 1
  fi
  if [ ! -f "$db" ]; then
    echo "FAIL: warm-up did not create the isolated registry DB: $db" >&2
    return 1
  fi
  python3 "$HERE/lib/seed-registry.py" "$db" "$root" "$count"
}
