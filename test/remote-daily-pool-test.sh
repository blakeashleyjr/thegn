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
for step in "P0" "P1" "P2" "P3" "P4" "P5"; do
  grep -Fq "$step" <<<"$pool_plan"
done
grep -Fq -- "--with-pool" <<<"$pool_plan"

rc=0
THEGN_BIN="$TMP/stub" bash "$SCRIPT" --provider fly --with-pool >"$TMP/refused" 2>&1 || rc=$?
[ "$rc" -eq 2 ]
grep -Fq -- "--with-pool is supported only with --provider sprites" "$TMP/refused"

rc=0
THEGN_CHANNEL=dev THEGN_BIN="$TMP/not-called" bash "$SCRIPT" --env x --i-accept-spend --with-pool >"$TMP/hold-refused" 2>&1 || rc=$?
[ "$rc" -eq 2 ]
grep -Fq -- "needs RD_HOLD_AGENT=1" "$TMP/hold-refused"

if grep -Fq 'export XDG_RUNTIME_DIR="$TMP/run"' "$SCRIPT"; then
  echo "FAIL: runtime dir override is back" >&2
  exit 1
fi
if grep -Fq 'mkdir -m 700 "$TMP/run"' "$SCRIPT"; then
  echo "FAIL: runtime dir mkdir is back" >&2
  exit 1
fi
[ ! -e "$TMP/called" ]

echo "remote-daily-pool-test: PASS"
