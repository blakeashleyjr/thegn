#!/usr/bin/env bash
# Bisect which nextest binary leaks `podman pause` processes (THE-870).
#
# Human/primary-run only: not wired into just or CI. NEVER kills anything; it
# prints the kill command for pids attributed to its own private XDG_RUNTIME_DIR.
#
# Usage: test/podman-pause-bisect.sh [--list] [--binary <binary-id>] [--narrow <binary-id>]
#        test/podman-pause-bisect.sh --self-test
#   --list            list test binaries and exit (nothing is run)
#   --binary <id>     only run that nextest binary id
#   --narrow <id>     repeat per test name inside that binary
#   --self-test       fixture-test the attribution against a fake PROC_ROOT
# Env: PROC_ROOT (default /proc). Linux only; exits 0 with a message elsewhere.

set -euo pipefail

PROC_ROOT="${PROC_ROOT:-/proc}"

# Print pids whose comm is `podman pause` (or `pause`).
bisect_pause_pids() {
  local d comm
  [ -d "$PROC_ROOT" ] || return 0
  for d in "$PROC_ROOT"/[0-9]*; do
    [ -d "$d" ] || continue
    if IFS= read -r comm 2>/dev/null <"$d/comm" && { [ "$comm" = 'podman pause' ] || [ "$comm" = pause ]; }; then
      printf '%s\n' "${d##*/}"
    fi
  done
}

# bisect_env_var <pid> <NAME>: value from environ, empty if unreadable/absent.
bisect_env_var() {
  local pid="$1" name="$2" entry
  [ -r "$PROC_ROOT/$pid/environ" ] || return 0
  while IFS= read -r -d '' entry; do
    if [ "${entry%%=*}" = "$name" ]; then
      printf '%s' "${entry#*=}"
      return 0
    fi
  done <"$PROC_ROOT/$pid/environ" 2>/dev/null || true
}

# Print "pid<TAB>ppid<TAB>cwd<TAB>runtime-dir" for every pause pid.
bisect_snapshot() {
  local pid ppid cwd rd
  while IFS= read -r pid; do
    ppid="$(sed -n 's/^PPid:[[:space:]]*//p' "$PROC_ROOT/$pid/status" 2>/dev/null || true)"
    cwd="$(readlink "$PROC_ROOT/$pid/cwd" 2>/dev/null || true)"
    rd="$(bisect_env_var "$pid" XDG_RUNTIME_DIR)"
    printf '%s\t%s\t%s\t%s\n' "$pid" "${ppid:-?}" "${cwd:-?}" "${rd:-?}"
  done < <(bisect_pause_pids)
}

# bisect_attribute <runtime-dir>: snapshot rows whose runtime dir is exactly it.
bisect_attribute() {
  local want="$1" pid ppid cwd rd
  while IFS=$'\t' read -r pid ppid cwd rd; do
    [ -n "$pid" ] || continue
    if [ "$rd" = "$want" ]; then
      printf '%s\t%s\t%s\n' "$pid" "$ppid" "$cwd"
    fi
  done < <(bisect_snapshot)
}

bisect_self_test() {
  local root fail=0 out
  root="$(mktemp -d "${TMPDIR:-/tmp}/podman-bisect-selftest.XXXXXX")"
  PROC_ROOT="$root"
  mk() { # pid comm runtime-dir cwd
    mkdir -p "$root/$1"
    printf '%s\n' "$2" >"$root/$1/comm"
    printf 'Name:\t%s\nPPid:\t1\n' "$2" >"$root/$1/status"
    printf 'A=b\0XDG_RUNTIME_DIR=%s\0Z=y\0' "$3" >"$root/$1/environ"
    ln -s "$4" "$root/$1/cwd"
  }
  mk 101 'podman pause' /tmp/priv /work/a
  mk 102 'pause' /tmp/other /work/b
  mk 103 'bash' /tmp/priv /work/c
  mk 104 'podman pause' /tmp/priv /work/d
  out="$(bisect_pause_pids | sort | tr '\n' ' ')"
  [ "$out" = '101 102 104 ' ] || {
    printf 'FAIL pids: %s\n' "$out"
    fail=1
  }
  out="$(bisect_attribute /tmp/priv | cut -f1 | sort | tr '\n' ' ')"
  [ "$out" = '101 104 ' ] || {
    printf 'FAIL attribute: %s\n' "$out"
    fail=1
  }
  out="$(bisect_attribute /tmp/priv | grep -c '^101'"$(printf '\t')"'1'"$(printf '\t')"'/work/a$' || true)"
  [ "$out" = 1 ] || {
    printf 'FAIL fields: %s\n' "$out"
    fail=1
  }
  [ -z "$(bisect_attribute /tmp/none)" ] || {
    printf 'FAIL none\n'
    fail=1
  }
  rm -rf "$root"
  if [ "$fail" = 0 ]; then
    printf 'self-test: ok\n'
  fi
  return "$fail"
}

# bisect_run <label> <filterset>: run nextest privately, print/record leaks.
LEAKERS=()
bisect_run() {
  local label="$1" filter="$2" priv rows n ch rh rc=0
  priv="$(mktemp -d "${TMPDIR:-/tmp}/podman-bisect.XXXXXX")"
  mkdir -p "$priv/run" "$priv/home"
  chmod 700 "$priv/run"
  ch="${CARGO_HOME:-$HOME/.cargo}"
  rh="${RUSTUP_HOME:-$HOME/.rustup}"
  env CARGO_HOME="$ch" RUSTUP_HOME="$rh" HOME="$priv/home" XDG_RUNTIME_DIR="$priv/run" \
    cargo nextest run --workspace -E "$filter" --no-fail-fast >"$priv/out.log" 2>&1 || rc=$?
  # 0 = all passed, 100 = test failures (both normal). 4 = nextest matched no
  # tests; anything else is a non-test error. Warn, but keep the harness going.
  case "$rc" in
  0 | 100) ;;
  4) printf 'WARNING: %s: nextest matched no tests (filter: %s); count is not evidence (see %s/out.log)\n' "$label" "$filter" "$priv" ;;
  *) printf 'WARNING: %s: nextest exited %s (non-test error); count is not evidence (see %s/out.log)\n' "$label" "$rc" "$priv" ;;
  esac
  rows="$(bisect_attribute "$priv/run")"
  if [ -z "$rows" ]; then
    printf '%-60s 0\n' "$label"
  else
    n="$(printf '%s\n' "$rows" | wc -l)"
    printf '%-60s %s\n' "$label" "$n"
    printf '%s\n' "$rows" | while IFS=$'\t' read -r pid ppid cwd; do
      printf '    pid=%s ppid=%s cwd=%s\n' "$pid" "$ppid" "$cwd"
    done
    printf '    to reap (human only): kill %s\n' "$(printf '%s\n' "$rows" | cut -f1 | tr '\n' ' ')"
    LEAKERS+=("$label")
  fi
  printf '    log: %s/out.log\n' "$priv"
}

main() {
  local list_only=0 binary='' narrow='' bins b tests t
  while [ $# -gt 0 ]; do
    case "$1" in
    --list) list_only=1 ;;
    --binary)
      binary="${2:?--binary needs an id}"
      shift
      ;;
    --narrow)
      narrow="${2:?--narrow needs an id}"
      shift
      ;;
    --self-test)
      bisect_self_test
      return
      ;;
    -h | --help)
      sed -n '2,15p' "$0"
      return 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      return 2
      ;;
    esac
    shift
  done

  if [ "$PROC_ROOT" = /proc ] && [ "$(uname -s)" != Linux ]; then
    printf 'podman-pause-bisect: Linux only (needs /proc); nothing to do.\n'
    return 0
  fi
  if [ ! -d "$PROC_ROOT" ] || { [ "$PROC_ROOT" = /proc ] && [ ! -r /proc/self/comm ]; }; then
    printf 'podman-pause-bisect: cannot read %s; refusing to run.\n' "$PROC_ROOT" >&2
    return 1
  fi

  local base
  base="$(bisect_snapshot)"
  printf 'baseline: %s podman pause process(es)\n' "$(printf '%s' "$base" | grep -c . || true)"
  [ -z "$base" ] || printf '%s\n' "$base" | sed 's/^/  /'

  tests="$(cargo nextest list --workspace --message-format oneline 2>/dev/null)"
  bins="$(printf '%s\n' "$tests" | awk 'NF>=2 {print $1}' | sort -u)"
  if [ "$list_only" = 1 ]; then
    printf '%s\n' "$bins"
    return 0
  fi

  printf '%-60s %s\n' BINARY NEW-PAUSE-PIDS
  if [ -n "$narrow" ]; then
    while IFS= read -r t; do
      [ -n "$t" ] || continue
      bisect_run "$narrow :: $t" "binary_id(=$narrow) & test(=$t)"
    done < <(printf '%s\n' "$tests" | awk -v b="$narrow" '$1==b {print $2}')
  else
    while IFS= read -r b; do
      [ -n "$b" ] || continue
      if [ -n "$binary" ] && [ "$b" != "$binary" ]; then
        continue
      fi
      bisect_run "$b" "binary_id(=$b)"
    done <<<"$bins"
  fi

  printf '\nleaking:\n'
  if [ "${#LEAKERS[@]}" -eq 0 ]; then
    printf '  (none)\n'
  else
    printf '  %s\n' "${LEAKERS[@]}"
  fi
}

main "$@"
