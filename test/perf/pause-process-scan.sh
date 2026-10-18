#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf -- "$TMP"' EXIT
PERF_PROC_ROOT="$TMP/proc"
PERF_TMP="$TMP/fixture"
mkdir -p "$PERF_TMP"
mkdir -p "$PERF_PROC_ROOT/101" "$PERF_PROC_ROOT/102" "$PERF_PROC_ROOT/103"
printf 'podman pause\n' >"$PERF_PROC_ROOT/101/comm"
printf 'podman\n' >"$PERF_PROC_ROOT/102/comm"
# PID 103 vanished after enumeration, so its comm file is absent.
# shellcheck source=test/perf/lib/env.sh disable=SC1091
source "$HERE/lib/env.sh"

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
if [ -s "$stderr" ]; then
  cat "$stderr" >&2
  printf 'cleanup wrote an error for a vanished PID\n' >&2
  exit 1
fi
