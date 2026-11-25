#!/usr/bin/env bash
# Resident-set sampling for the perf harness.
#
# Kept in a lib rather than inline in cpu-sample.sh so the parsing can be
# tested without launching thegn: `PERF_PROC_ROOT` points the reader at a
# fixture tree (the same hook test/perf/pause-process-scan.sh uses), and the
# `ps` fallback can be exercised with a stub on PATH.

# Per-process resident memory in KiB, or 0 when it cannot be read.
#
# `/proc/<pid>/status` rather than `/proc/<pid>/statm`: VmRSS is already in KiB,
# so there is no page-size assumption (Linux arm64 kernels may use 16 or 64 KiB
# pages, and booting the same binary on macOS has no procfs at all). A process
# that vanished between enumeration and the read answers 0 rather than an empty
# string, because every caller compares the result numerically and `[ "" -gt 0 ]`
# is a shell error, not a measurement.
proc_rss_kb() { # $1 = pid -> KiB resident
  local pid="${1:-}" kb="" root="${PERF_PROC_ROOT:-/proc}"
  [ -n "$pid" ] || {
    printf '0'
    return 0
  }
  if [ -r "$root/$pid/status" ]; then
    kb="$(awk '/^VmRSS:/ { print $2; exit }' "$root/$pid/status" 2>/dev/null || true)"
  else
    # No procfs (macOS): `ps -o rss=` reports KiB as well.
    kb="$(ps -o rss= -p "$pid" 2>/dev/null | awk 'NF { print $1 + 0; exit }' || true)"
  fi
  # A kernel thread has no VmRSS line and a truncated read has no field at all.
  case "$kb" in
  '' | *[!0-9]*) kb=0 ;;
  esac
  printf '%s' "$kb"
}
