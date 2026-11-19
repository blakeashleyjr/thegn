#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$ROOT/test/remote-daily.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

plan="$(THEGN_BIN="$TMP/not-called" bash "$SCRIPT")"
printf '%s\n' "$plan" | grep -Fq "fresh XDG_STATE_HOME and THEGN_DIR"
[ ! -e "$TMP/not-called" ]

pool_plan="$(THEGN_BIN="$TMP/not-called" bash "$SCRIPT" --with-pool)"
for step in "P0" "P1" "P2" "P3" "P4" "P5"; do
  printf '%s\n' "$pool_plan" | grep -Fq "$step"
done
printf '%s\n' "$pool_plan" | grep -Fq -- "--with-pool"

rc=0
THEGN_BIN="$TMP/not-called" bash "$SCRIPT" --provider fly --with-pool >"$TMP/refused" 2>&1 || rc=$?
[ "$rc" -eq 2 ]
grep -Fq -- "--with-pool is supported only with --provider sprites" "$TMP/refused"

! grep -Fq 'export XDG_RUNTIME_DIR="$TMP/run"' "$SCRIPT"
! grep -Fq 'mkdir -m 700 "$TMP/run"' "$SCRIPT"

echo "remote-daily-pool-test: PASS"
