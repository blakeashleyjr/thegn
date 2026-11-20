#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$ROOT/test/remote-daily.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

printf '#!/bin/sh\ntouch "%s/called"\nexit 1\n' "$TMP" >"$TMP/stub"
chmod +x "$TMP/stub"

plan="$(THEGN_BIN="$TMP/stub" bash "$SCRIPT")"
grep -Fq "fresh XDG_STATE_HOME and THEGN_DIR" <<<"$plan"

pool_plan="$(THEGN_BIN="$TMP/stub" bash "$SCRIPT" --with-pool)"
for step in "11o" "11g" "11e" "11f" "11n" "11d" "11r" "11q" "P0" "P1" "P2" "P3" "P4" "P5"; do
  grep -Fq "$step" <<<"$pool_plan"
done
grep -Fq -- "--with-pool" <<<"$pool_plan"
grep -Fq -- "otherwise env up --worktree <rd-p path>" <<<"$pool_plan"

rc=0
THEGN_BIN="$TMP/stub" bash "$SCRIPT" --provider fly --with-pool >"$TMP/refused" 2>&1 || rc=$?
[ "$rc" -eq 2 ]
grep -Fq -- "--with-pool is supported only with --provider sprites" "$TMP/refused"

# shellcheck disable=SC2016 # literal text searched for in the script
if grep -Fq 'export XDG_RUNTIME_DIR="$TMP/run"' "$SCRIPT"; then
  echo "FAIL: runtime dir override is back" >&2
  exit 1
fi
# shellcheck disable=SC2016 # literal text searched for in the script
if grep -Fq 'mkdir -m 700 "$TMP/run"' "$SCRIPT"; then
  echo "FAIL: runtime dir mkdir is back" >&2
  exit 1
fi
grep -Fq 'RD_OVERLAY_AGENT' "$SCRIPT"
grep -Fq 'agent list' "$SCRIPT"
grep -Fq 'daemon >"$TMP/overlay-daemon.log"' "$SCRIPT"
grep -Fq 'no_stdin "$THEGN_BIN" env up --worktree "$POOL_PATH"' "$SCRIPT"
grep -Fq 'rd-y2' "$SCRIPT"
[ ! -e "$TMP/called" ]

echo "remote-daily-pool-test: PASS"
