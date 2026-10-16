#!/usr/bin/env bash
# Hermetic self-test for scripts/test-remote.sh (`just test-remote-selftest`).
#
# No network and no real ssh: THEGN_REMOTE_SSH points at a shim that runs the
# "remote" command locally, in a throwaway HOME. That still exercises every
# step the runner owns — packing, the remote-dir layout, the snapshot commit,
# exit-status propagation and cleanup — which are the parts that break
# silently. What it cannot check is a real sshd or a real Windows shell; those
# need `just test-remote` against a real host.
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/thegn-remote-selftest.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
fakehome="$tmp/home"
mkdir -p "$fakehome"

# The shim: drop ssh options, take the first operand as the host and the rest
# as the remote command line, which ssh would hand to the login shell.
cat >"$tmp/ssh" <<'SHIM'
#!/usr/bin/env bash
set -u
control=""
control_path=""
while [ $# -gt 0 ]; do
  case "$1" in
  -o)
    case "$2" in ControlPath=*) control_path=${2#ControlPath=} ;; esac
    shift 2
    ;;
  -F) shift 2 ;;
  -O) control=$2; shift 2 ;;
  -t | -tt) shift ;;
  *) break ;;
  esac
done
if [ -n "$control_path" ] && [ -n "${CONTROL_PATH_LOG:-}" ]; then
  printf '%s\n' "$control_path" >>"$CONTROL_PATH_LOG"
fi
shift # host
[ -n "$control" ] && exit 0
cd "$FAKE_HOME" && exec sh -c "$*"
SHIM
chmod +x "$tmp/ssh"

fail() {
  echo "remote-runner-selftest: FAIL: $*" >&2
  exit 1
}

run() {
  FAKE_HOME=$fakehome CONTROL_PATH_LOG="$tmp/control-paths" THEGN_REMOTE_SSH="$tmp/ssh" THEGN_REMOTE_DIR=rt \
    "$repo/scripts/test-remote.sh" host=fake
}

# 1. the gate runs in a synced snapshot and its exit status comes back.
set +e
# shellcheck disable=SC2016 # expanded by the remote shell, not here
THEGN_REMOTE_CMD='test -f Cargo.toml && test ! -e target && git log --oneline -1 | grep -q "test-remote snapshot" && echo "TARGET=$CARGO_TARGET_DIR" && exit 7' \
  run >"$tmp/out1" 2>&1
rc=$?
set -e
[ "$rc" -eq 7 ] || {
  cat "$tmp/out1" >&2
  fail "expected the remote gate's exit 7, got $rc"
}
grep -q "TARGET=$fakehome/rt/target" "$tmp/out1" || fail "CARGO_TARGET_DIR was not <remote-dir>/target"
control_path=$(head -n 1 "$tmp/control-paths")
[ "${control_path#/tmp/tgr.}" != "$control_path" ] || fail "control socket did not use the short /tmp directory"
[ "${#control_path}" -lt 104 ] || fail "control socket path exceeds the Unix socket path limit"
[ -d "$fakehome/rt" ] || fail "remote dir missing after a default run"
[ ! -e "$fakehome/rt/src" ] || fail "remote src/ not cleaned up"
[ ! -e "$fakehome/rt/run.sh" ] || fail "remote run.sh not cleaned up"

# 2. a passing gate is 0.
THEGN_REMOTE_CMD='exit 0' run >"$tmp/out2" 2>&1 || fail "a passing gate did not exit 0"

# 3. THEGN_REMOTE_CLEAN=1 removes the whole remote dir, target/ included.
mkdir -p "$fakehome/rt/target"
THEGN_REMOTE_CLEAN=1 THEGN_REMOTE_CMD='exit 0' run >"$tmp/out3" 2>&1 || fail "clean run failed"
[ ! -e "$fakehome/rt" ] || fail "THEGN_REMOTE_CLEAN=1 left the remote dir"

# 4. an unsafe remote dir is refused before anything is shipped.
for bad_dir in 'a b;rm' / /Users ../outside a/../outside a/./b; do
  set +e
  FAKE_HOME=$fakehome THEGN_REMOTE_SSH="$tmp/ssh" THEGN_REMOTE_DIR="$bad_dir" THEGN_REMOTE_OS=unix \
    "$repo/scripts/test-remote.sh" fake >"$tmp/out4" 2>&1
  rc=$?
  set -e
  [ "$rc" -eq 2 ] || fail "unsafe THEGN_REMOTE_DIR '$bad_dir' was not refused (rc=$rc)"
done

# 5. destroy refuses unmarked state and protected roots without deleting them.
mkdir -p "$fakehome/unmarked-state"
set +e
THEGN_WINVM_DIR="$fakehome/unmarked-state" HOME="$fakehome" "$repo/scripts/windows-vm.sh" destroy >"$tmp/out5" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "destroy accepted an unmarked state directory"
mkdir -p "$fakehome/marked-home"
touch "$fakehome/marked-home/windows-11.conf"
set +e
THEGN_WINVM_DIR="$fakehome/marked-home" HOME="$fakehome/marked-home" "$repo/scripts/windows-vm.sh" destroy >"$tmp/out6" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "destroy accepted HOME even with a VM marker"
[ -f "$fakehome/marked-home/windows-11.conf" ] || fail "destroy removed the protected home marker"

echo "remote-runner-selftest: ok ($(grep -o '[0-9]* files' "$tmp/out1"))"
