#!/usr/bin/env bash
# test/gen-db-fixtures.sh — capture small, seeded `thegn.db` snapshots from
# shipped releases into crates/thegn-core/tests/fixtures/db/<tag>.db, replayed
# by crates/thegn-core/tests/db_upgrade_from_release.rs (THE-2656).
#
# Each release is built from its git tag in a throwaway worktree (slow: run it
# on the build Mac, not the laptop), launched against an isolated XDG_STATE_HOME
# so it creates and migrates a DB, then seeded through that release's own CLI.
# We then `VACUUM INTO` a compact copy. Re-run only when adding a rung.
#
# Usage: test/gen-db-fixtures.sh [tag-suffix...]   (default: alpha.100 alpha.120 alpha.131)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/crates/thegn-core/tests/fixtures/db"
mkdir -p "$OUT"
RUNGS=("$@")
[ ${#RUNGS[@]} -gt 0 ] || RUNGS=(alpha.100 alpha.120 alpha.131)
WORK="$(mktemp -d)"
trap 'git -C "$ROOT" worktree prune; rm -rf "$WORK"' EXIT

for rung in "${RUNGS[@]}"; do
  tag="v0.1.0-$rung"
  wt="$WORK/$rung"
  state="$WORK/state-$rung"
  mkdir -p "$state"
  git -C "$ROOT" worktree add --detach "$wt" "$tag"
  (cd "$wt" && cargo build --release -p thegn)
  bin="$wt/target/release/thegn"
  repo="$WORK/repo-$rung"
  git init -q "$repo" && git -C "$repo" -c commit.gpgsign=false \
    commit -q --allow-empty -m seed
  # Seed worktrees/sessions/layout with the release's own CLI so the rows are
  # exactly what that release writes.
  (cd "$repo" && XDG_STATE_HOME="$state" XDG_CONFIG_HOME="$WORK/cfg" HOME="$WORK/home" \
    "$bin" wt new seed-a && "$bin" wt new seed-b)
  sqlite3 "$state/thegn/thegn.db" "PRAGMA wal_checkpoint(TRUNCATE); VACUUM INTO '$OUT/$rung.db';"
  echo "wrote $OUT/$rung.db (user_version $(sqlite3 "$OUT/$rung.db" 'PRAGMA user_version'))"
done
