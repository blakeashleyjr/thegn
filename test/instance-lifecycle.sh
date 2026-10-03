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
set +m # no "Killed" job notices
fail() {
  echo "FAIL: $*" >&2
  exit 1
}
# A killed-but-unreaped child (zombie) is dead for our purposes.
alive() {
  kill -0 "$1" 2>/dev/null || return 1
  ! grep -q '^State:[[:space:]]*Z' "/proc/$1/status" 2>/dev/null
}
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
  inst_stop "$tmp/bad$n.pid" 1 2>/dev/null || true # refusing a live malformed pid is fine
  alive "$sentinel" || fail "malformed pidfile #$n signalled the sentinel"
  [ -e "$tmp/bad$n.pid" ] || fail "malformed pidfile #$n removed"
done

# 3. Symlinked pidfile is refused.
echo "$sentinel $(inst_gen "$sentinel")" >"$tmp/real"
ln -s "$tmp/real" "$tmp/link.pid"
inst_stop "$tmp/link.pid" 1 2>/dev/null || true
alive "$sentinel" || fail "symlinked pidfile signalled the sentinel"

# 4. The exact recorded generation IS stopped (bounded), including SIGTERM-ignoring.
bash -c 'trap "" TERM; while :; do sleep 0.2; done' &
stubborn=$!
pids+=("$stubborn")
disown "$stubborn" # no bash "Killed" job notice when the lib escalates
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
rp=""
for _ in 1 2 3; do
  spawn
  (inst_restart "$tmp/c.pid" "$SPAWNED") &
  rp="$rp $!"
done
# shellcheck disable=SC2086 # word-split the pid list on purpose
wait $rp
inst_parse_meta "$tmp/c.pid" || fail "concurrent launch corrupted pidfile"

# 5b. Legacy bare-pid file: ours (state-root env + thegn argv0) is stopped; an
# unrelated live pid refuses the launch.
spawn
unrelated=$SPAWNED
echo "$unrelated" >"$tmp/legacy.pid"
if INST_STATE="$tmp/none" inst_stop "$tmp/legacy.pid" 1 2>/dev/null; then fail "unrelated legacy pid did not refuse launch"; fi
alive "$unrelated" || fail "unrelated legacy pid was signalled"

if [ -d /proc/self ]; then
  # fake_thegn STATE_ROOT: a process with argv `<tmp>/thegn daemon`, running with XDG_STATE_HOME=STATE_ROOT.
  cp "$(command -v bash)" "$tmp/thegn"
  printf 'while :; do sleep 1; done\n' >"$tmp/daemon"
  fake_thegn() {
    (
      cd "$tmp"
      XDG_STATE_HOME=$1 exec perl -e 'exec {"bash"} ("'"$tmp"'/thegn", "daemon")'
    ) &
    FAKE=$!
    pids+=("$FAKE")
    disown "$FAKE"
    # Wait for the exec to land (a loaded box can take seconds), not a fixed nap.
    for _ in $(seq 100); do
      [ "$(tr '\0' '\n' <"/proc/$FAKE/cmdline" 2>/dev/null | sed -n 1p)" = "$tmp/thegn" ] && break
      sleep 0.1
    done
  }

  # 5c. legacy pid that IS our thegn gets stopped
  mkdir -p "$tmp/state-l"
  fake_thegn "$tmp/state-l"
  echo "$FAKE" >"$tmp/legacy2.pid"
  INST_STATE="$tmp/state-l" inst_stop "$tmp/legacy2.pid" 1
  alive "$FAKE" && fail "verified legacy pid not stopped"

  # 6. Scoped rotation: only the selected state root's daemon dies.
  sa="$tmp/state-a"
  sb="$tmp/state-b"
  mkdir -p "$sa" "$sb"
  fake_thegn "$sa"
  da=$FAKE
  fake_thegn "$sb"
  db=$FAKE
  inst_rotate_scoped "$sa"
  sleep 0.3
  alive "$da" && fail "selected state root's daemon survived rotation"
  alive "$db" || fail "rotation killed another state root's daemon"
  alive "$sentinel" || fail "rotation killed an unrelated process"
fi

[ -d /proc/self ] || echo "skip: scoped rotation and legacy-ours checks (no /proc)"
echo "instance lifecycle checks passed"
