#!/usr/bin/env bash
# Behavioural tests for test/lib/instance.sh (THE-436): the launcher must only
# signal the exact process generation it recorded, and rotation must stay inside
# one state root.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=test/lib/instance.sh disable=SC1091
. "$repo/test/lib/instance.sh"
tmp="$(mktemp -d)"
pids=()
cleanup() {
  for p in "${pids[@]:-}"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null || true; done
  rm -rf "$tmp"
}
trap cleanup EXIT
fail() {
  echo "FAIL: $*" >&2
  exit 1
}
alive() { kill -0 "$1" 2>/dev/null; }
spawn() {
  sleep 300 &
  pids+=("$!")
  SPAWNED=$!
}

# 1. Numeric pid reuse: sentinel carries a DIFFERENT generation than recorded.
spawn
sentinel=$SPAWNED
echo "$sentinel 1" >"$tmp/reuse.pid"
inst_stop "$tmp/reuse.pid" 1
alive "$sentinel" || fail "reused pid was signalled"
[ ! -e "$tmp/reuse.pid" ] || fail "stale generation state not removed"

# 2. Malformed metadata is never signalled and is left in place.
n=0
for bad in "0 x" "-5 x" "99999999999 x" "$sentinel" "${sentinel}abc x" "$sentinel x!" "" "$sentinel x
$sentinel y"; do
  n=$((n + 1))
  printf '%s' "$bad" >"$tmp/bad$n.pid"
  inst_stop "$tmp/bad$n.pid" 1 2>/dev/null
  alive "$sentinel" || fail "malformed pidfile #$n signalled the sentinel"
  [ -e "$tmp/bad$n.pid" ] || fail "malformed pidfile #$n removed"
done

# 3. Symlinked pidfile is refused.
echo "$sentinel $(inst_gen "$sentinel")" >"$tmp/real"
ln -s "$tmp/real" "$tmp/link.pid"
inst_stop "$tmp/link.pid" 1 2>/dev/null
alive "$sentinel" || fail "symlinked pidfile signalled the sentinel"

# 4. The exact recorded generation IS stopped (bounded), including SIGTERM-ignoring.
bash -c 'trap "" TERM; while :; do sleep 0.2; done' &
stubborn=$!
pids+=("$stubborn")
sleep 0.2
inst_write_meta "$tmp/own.pid" "$stubborn"
inst_parse_meta "$tmp/own.pid" || fail "own metadata rejected"
inst_stop "$tmp/own.pid" 1
alive "$stubborn" && fail "owned generation survived"
[ ! -e "$tmp/own.pid" ] || fail "state not cleared after exit"
wait "$stubborn" 2>/dev/null || true

# 5. Concurrent launchers serialize and leave a valid record.
spawn
one=$SPAWNED
inst_write_meta "$tmp/c.pid" "$one"
for _ in 1 2 3; do
  spawn
  (inst_restart "$tmp/c.pid" "$SPAWNED") &
done
wait %2 %3 %4 2>/dev/null || true
inst_parse_meta "$tmp/c.pid" || fail "concurrent launch corrupted pidfile"

# 6. Scoped rotation: only the selected state root's daemon dies.
sa="$tmp/state-a"
sb="$tmp/state-b"
mkdir -p "$sa" "$sb"
printf '#!/bin/sh\nwhile :; do sleep 1; done\n' >"$tmp/thegn"
chmod +x "$tmp/thegn"
env XDG_STATE_HOME="$sa" "$tmp/thegn" daemon &
da=$!
pids+=("$da")
env XDG_STATE_HOME="$sb" "$tmp/thegn" daemon &
db=$!
pids+=("$db")
sleep 0.3
inst_rotate_scoped "$sa"
sleep 0.3
alive "$da" && fail "selected state root's daemon survived rotation"
alive "$db" || fail "rotation killed another state root's daemon"
alive "$sentinel" || fail "rotation killed an unrelated process"
wait "$da" 2>/dev/null || true
echo "instance lifecycle checks passed"
