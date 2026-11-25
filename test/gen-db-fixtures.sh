#!/usr/bin/env bash
# test/gen-db-fixtures.sh — capture small, seeded `thegn.db` snapshots from
# shipped releases into crates/thegn-core/tests/fixtures/db/<tag>.db, replayed
# by crates/thegn-core/tests/db_upgrade_from_release.rs (THE-2656).
#
# Each rung uses that release's published Linux x86_64 musl binary (downloaded
# with `gh release download`, no build), launched against an isolated
# XDG_STATE_HOME so it creates and migrates a DB, then seeded through that
# release's own CLI. We then `VACUUM INTO` a compact copy. Re-run only when
# adding a rung; a rung must be a published (non-draft) release.
#
# Usage: test/gen-db-fixtures.sh [tag-suffix...]   (default: alpha.119 alpha.125 alpha.131)
# Env:   OUT=<dir> to write elsewhere (default: the fixtures directory).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${OUT:-$ROOT/crates/thegn-core/tests/fixtures/db}"
mkdir -p "$OUT"
RUNGS=("$@")
[ ${#RUNGS[@]} -gt 0 ] || RUNGS=(alpha.119 alpha.125 alpha.131)
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

for rung in "${RUNGS[@]}"; do
  tag="v0.1.0-$rung"
  dl="$WORK/dl-$rung"
  state="$WORK/state-$rung"
  mkdir -p "$dl" "$state"
  gh release download "$tag" -R blakeashleyjr/thegn -D "$dl" \
    -p "thegn-$tag-x86_64-unknown-linux-musl.tar.gz"
  tar -xzf "$dl"/*.tar.gz -C "$dl"
  bin="$(find "$dl" -type f -name thegn -perm -u+x | head -1)"
  [ -n "$bin" ] || {
    echo "no thegn binary in $tag archive" >&2
    exit 1
  }
  repo="$WORK/repo-$rung"
  git init -q "$repo" && git -C "$repo" -c commit.gpgsign=false \
    commit -q --allow-empty -m seed
  # Seed worktrees/sessions/layout with the release's own CLI so the rows are
  # exactly what that release writes.
  (cd "$repo" && export XDG_STATE_HOME="$state" XDG_CONFIG_HOME="$WORK/cfg" \
    XDG_DATA_HOME="$WORK/data-$rung" XDG_RUNTIME_DIR="$WORK/run-$rung" HOME="$WORK/home" &&
    mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR" &&
    "$bin" wt new seed-a && "$bin" wt new seed-b)
  sqlite3 "$state/thegn/thegn.db" "PRAGMA wal_checkpoint(TRUNCATE); VACUUM INTO '$OUT/$rung.db';"
  echo "wrote $OUT/$rung.db (user_version $(sqlite3 "$OUT/$rung.db" 'PRAGMA user_version'))"
done
