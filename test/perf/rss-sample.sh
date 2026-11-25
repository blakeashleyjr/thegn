#!/usr/bin/env bash
# Hermetic test for the perf harness RSS reader (THE-2664).
#
# No thegn, no real process: `PERF_PROC_ROOT` points `proc_rss_kb` at a fixture
# tree, and a stub `ps` on PATH exercises the no-procfs fallback. This is the
# parsing half of the RSS axis, which is the half that can silently report a
# wrong number; the sampling loop is bounded arithmetic over the same reader.
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf -- "$TMP"' EXIT

PERF_PROC_ROOT="$TMP/proc"
mkdir -p "$PERF_PROC_ROOT/101" "$PERF_PROC_ROOT/102" "$PERF_PROC_ROOT/103" \
  "$PERF_PROC_ROOT/104" "$PERF_PROC_ROOT/202" "$PERF_PROC_ROOT/203"
# VmRSS is already KiB, so no page-size assumption is needed.
printf 'Name:\tthegn\nVmRSS:\t  12345 kB\n' >"$PERF_PROC_ROOT/101/status"
# A kernel thread carries no VmRSS line.
printf 'Name:\tkworker/0\n' >"$PERF_PROC_ROOT/102/status"
# A read race can expose a partial field; a non-numeric value is not a measurement.
printf 'Name:\tthegn\nVmRSS:\t unknown kB\n' >"$PERF_PROC_ROOT/104/status"
# 103 and the pids below have no status file: they take the `ps` path.

# shellcheck source=test/perf/lib/rss.sh disable=SC1091
source "$HERE/lib/rss.sh"

expect() { # $1 = expected KiB, $2 = case, $3 = pid
  local actual
  actual="$(proc_rss_kb "$3")"
  if [ "$actual" != "$1" ]; then
    printf 'proc_rss_kb(%s)=%s, want %s (%s)\n' "$3" "$actual" "$1" "$2" >&2
    exit 1
  fi
}

expect 12345 'VmRSS in KiB' 101
expect 0 'kernel thread without VmRSS' 102
expect 0 'vanished process' 999
expect 0 'empty pid' ''
expect 0 'non-numeric VmRSS' 104

# No procfs entry: the reader must fall back to `ps -o rss=`, which reports KiB
# too. Stub it rather than depending on a live process.
mkdir -p "$TMP/bin"
cat >"$TMP/bin/ps" <<'SHIM'
#!/usr/bin/env bash
printf '  4242\n'
SHIM
chmod +x "$TMP/bin/ps"
PATH="$TMP/bin:$PATH"
expect 4242 'ps fallback in KiB' 202

# A `ps` that cannot answer (the process vanished between enumeration and read)
# must read as zero, not as an empty string every numeric comparison would choke on.
cat >"$TMP/bin/ps" <<'SHIM'
#!/usr/bin/env bash
exit 1
SHIM
chmod +x "$TMP/bin/ps"
expect 0 'ps failure' 203

# The JSON contract: the reader is useless if the result document never carries
# it. Assert on the RESULT template itself so a computed-but-unpublished value
# cannot pass (the failure this test was written after).
result="$(grep -m1 '^RESULT="{' "$HERE/cpu-sample.sh" || true)"
for key in rss_host_kb rss_daemon_kb; do
  case "$result" in
  *"$key"*) ;;
  *)
    printf 'cpu-sample.sh RESULT does not publish %s\n' "$key" >&2
    exit 1
    ;;
  esac
done
