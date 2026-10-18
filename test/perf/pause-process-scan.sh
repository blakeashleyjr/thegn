#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf -- "$TMP"' EXIT
PERF_PROC_ROOT="$TMP/proc"
PERF_TMP="$TMP/fixture"
HOME="$TMP/home"
XDG_DATA_HOME="$TMP/data"
mkdir -p "$PERF_TMP"
mkdir -p "$XDG_DATA_HOME/containers/storage"
mkdir -p "$PERF_PROC_ROOT/101" "$PERF_PROC_ROOT/102" "$PERF_PROC_ROOT/103"
printf 'podman pause\n' >"$PERF_PROC_ROOT/101/comm"
printf 'podman\n' >"$PERF_PROC_ROOT/102/comm"
# The fixture path is present only in PID 101's environment; cleanup must select
# it while leaving the other matching/nonmatching process untouched.
printf 'XDG_RUNTIME_DIR=%s/run\0' "$PERF_TMP" >"$PERF_PROC_ROOT/101/environ"
printf 'XDG_RUNTIME_DIR=/another/fixture/run\0' >"$PERF_PROC_ROOT/102/environ"
# PID 103 vanished after enumeration, so its comm file is absent.
# shellcheck source=test/perf/lib/env.sh disable=SC1091
source "$HERE/lib/env.sh"

# Keep this fixture safe even if run against the old implementation, which
# called the shell's `kill` directly instead of the overridable hook.
kill() { return 0; }

PERF_KILL_LOG="$TMP/cleanup.log"
PERF_KILL="$TMP/record-kill"
cat >"$PERF_KILL" <<'SHIM'
#!/usr/bin/env bash
printf 'kill %s\n' "$1" >>"$PERF_KILL_LOG"
SHIM
chmod +x "$PERF_KILL"
mkdir -p "$TMP/bin"
cat >"$TMP/bin/podman" <<'SHIM'
#!/usr/bin/env bash
printf 'podman' >>"$PERF_KILL_LOG"
printf ' <%s>' "$@" >>"$PERF_KILL_LOG"
printf '\n' >>"$PERF_KILL_LOG"
SHIM
chmod +x "$TMP/bin/podman"
export PERF_KILL PERF_KILL_LOG PATH="$TMP/bin:$PATH"

stderr="$TMP/stderr"
actual="$(perf_pause_pids 2>"$stderr")"
if [ "$actual" != 101 ]; then
  printf 'expected only PID 101, got <%s>\n' "$actual" >&2
  exit 1
fi
if [ -s "$stderr" ]; then
  cat "$stderr" >&2
  printf 'pause scan wrote an error for a vanished PID\n' >&2
  exit 1
fi

perf_cleanup 2>"$stderr"
# The fake proc tree is static, so the post-delete rescan sees 101 again; its
# second kill proves that scan ran.
if [ "$(grep -c '^kill 101$' "$PERF_KILL_LOG")" -ne 2 ]; then
  printf 'expected both cleanup scans to kill only PID 101; log was <%s>\n' \
    "$(cat "$PERF_KILL_LOG" 2>/dev/null || true)" >&2
  exit 1
fi
if grep -F 'kill 102' "$PERF_KILL_LOG" >/dev/null; then
  printf 'cleanup killed PID 102, which does not belong to this fixture\n' >&2
  exit 1
fi
kill_line="$(grep -n -m 1 '^kill 101$' "$PERF_KILL_LOG" | cut -d: -f1)"
unshare_line="$(grep -n -m 1 '^podman <unshare> ' "$PERF_KILL_LOG" | cut -d: -f1)"
if [ -z "$unshare_line" ] || [ "$kill_line" -ge "$unshare_line" ]; then
  printf 'expected fixture pause kill before podman unshare; log was <%s>\n' \
    "$(cat "$PERF_KILL_LOG" 2>/dev/null || true)" >&2
  exit 1
fi
if [ -s "$stderr" ]; then
  cat "$stderr" >&2
  printf 'cleanup wrote an error for a vanished PID\n' >&2
  exit 1
fi

# Sandbox-on setup must stay isolated, and the fail-closed shim must win PATH
# lookup before this test attempts any systemctl command.
export TG_PERF_SANDBOX=1
perf_make_tmp
if [ -e "$XDG_RUNTIME_DIR/systemd/private" ] || [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
  printf 'sandbox-on setup exposed a systemd socket or session bus\n' >&2
  exit 1
fi
if ! grep -F 'cgroup_manager = "cgroupfs"' "$XDG_CONFIG_HOME/containers/containers.conf" >/dev/null ||
  ! grep -F 'events_logger = "file"' "$XDG_CONFIG_HOME/containers/containers.conf" >/dev/null; then
  printf 'sandbox-on Podman config did not select isolated cgroup/events backends\n' >&2
  exit 1
fi
if [ "$(command -v systemctl)" != "$PERF_TMP/bin/systemctl" ]; then
  printf 'sandbox-on systemctl shim is not first on PATH\n' >&2
  exit 1
fi
if [ "$(command -v systemd-run)" != "$PERF_TMP/bin/systemd-run" ]; then
  printf 'sandbox-on systemd-run shim is not first on PATH\n' >&2
  exit 1
fi
if systemctl --user show; then
  printf 'systemctl shim unexpectedly succeeded\n' >&2
  exit 1
fi
if systemd-run --user --scope true; then
  printf 'systemd-run shim unexpectedly succeeded\n' >&2
  exit 1
fi
if perf_check_systemd_calls 2>"$stderr"; then
  printf 'systemd-call log did not fail the perf check\n' >&2
  exit 1
fi
if ! grep -F 'systemctl <--user> <show>' "$PERF_TMP/systemd-calls.log" >/dev/null; then
  printf 'systemd shim did not record its argv\n' >&2
  exit 1
fi
if ! grep -F 'systemd-run <--user> <--scope> <true>' "$PERF_TMP/systemd-calls.log" >/dev/null; then
  printf 'systemd-run shim did not record its argv\n' >&2
  exit 1
fi
unset TG_PERF_SANDBOX
perf_cleanup
