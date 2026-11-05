#!/usr/bin/env bash
set -euo pipefail

# Opt-in Linux trace of a release binary's first frame. This is not part of
# `just ci`. It runs in a disposable HOME, XDG config/state/runtime/cache/data
# roots, a temporary git repository, and a DBus address that cannot reach a
# session bus. The default window ends immediately after the first frame;
# --hold-ms N instead runs the full loop for N milliseconds and includes later
# background work. strace's -e trace=network records connect and related
# network syscalls; the report counts connect() calls with an address and
# datagram sends to internet addresses. AF_UNIX is listed and is clean; every
# other family is reported and makes the command exit 1. Exit codes: 0 means
# the trace is valid and has no non-AF_UNIX target, 1 means a non-AF_UNIX
# connect or internet datagram target was observed, 2 means invalid usage,
# missing binary/strace, an empty trace, or a failed traced launch, and 3 means
# the host is not Linux.
#
# Known limitation: ptrace neutralises setuid, so setuid helper network use is
# invisible and rootful-podman probes are skipped. The result is a floor, not
# proof that no network is ever used. A first-frame window also says nothing
# about network use after that first frame.
#
# Report format ($OUT/net-trace.txt), UTF-8 with a final newline, in this order:
# net-trace.version: 1
# net-trace.binary: <path given to --bin>
# net-trace.window: first-frame | hold-ms <N>
# net-trace.syscalls_seen: <count of network-class syscall lines parsed>
# net-trace.connect_total: <count of connect() calls with an address>
# net-trace.connect_unix: <count with sa_family=AF_UNIX>
# net-trace.connect_other: <count with any other family, plus sendto/sendmsg to an inet address>
# net-trace.target.<K>: <FAMILY> <address[:port]> x<count> (K = 1.. in sorted order, only non-AF_UNIX targets)
# net-trace.unix.<K>: <sun_path or @abstract> x<count> (K = 1.. in sorted order)
# net-trace.verdict: clean | non-unix-connect | no-trace

usage() {
  cat <<'EOF'
Usage: bash scripts/first-run-net-trace.sh [--bin PATH] [--out DIR] [--hold-ms N] [--daemon] [-h|--help]
EOF
}

BIN="${THEGN_BIN:-target/release/thegn}"
OUT="${TMPDIR:-/tmp}/thegn-net-trace"
HOLD_MS=""
DAEMON=0

while (($#)); do
  case "$1" in
  --bin)
    if (($# < 2)); then
      usage >&2
      exit 2
    fi
    BIN="$2"
    shift 2
    ;;
  --out)
    if (($# < 2)); then
      usage >&2
      exit 2
    fi
    OUT="$2"
    shift 2
    ;;
  --hold-ms)
    if (($# < 2)); then
      usage >&2
      exit 2
    fi
    HOLD_MS="$2"
    shift 2
    ;;
  --daemon)
    DAEMON=1
    shift
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    printf 'net-trace: unknown argument: %s\n' "$1" >&2
    usage >&2
    exit 2
    ;;
  esac
done

if [[ -n $HOLD_MS ]]; then
  if [[ ! $HOLD_MS =~ ^[0-9]+$ ]] || [[ ${HOLD_MS//0/} == "" ]]; then
    printf 'net-trace: --hold-ms must be a positive integer\n' >&2
    exit 2
  fi
fi

if [[ "$(uname -s)" != Linux ]]; then
  printf 'net-trace: Linux only (needs strace)\n' >&2
  exit 3
fi

if [[ ! -x $BIN ]]; then
  printf 'net-trace: binary missing or not executable: %s\n' "$BIN" >&2
  exit 2
fi
if ! command -v strace >/dev/null 2>&1; then
  printf 'net-trace: requires strace; re-enter nix develop after the devShell change\n' >&2
  exit 2
fi
if ! command -v python3 >/dev/null 2>&1 || ! command -v git >/dev/null 2>&1; then
  printf 'net-trace: requires python3 and git\n' >&2
  exit 2
fi

BIN_LABEL="$BIN"
if [[ $BIN != /* ]]; then
  BIN="$PWD/$BIN"
fi
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=test/lib/pty.sh disable=SC1091
source "$REPO_ROOT/test/lib/pty.sh"

T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/home" "$T/cfg" "$T/state" "$T/run" "$T/cache" "$T/data" "$T/repo"
chmod 700 "$T/run"
: >"$T/gitconfig"
GIT_CONFIG_GLOBAL="$T/gitconfig" GIT_CONFIG_SYSTEM=/dev/null \
  git -C "$T/repo" -c init.defaultBranch=main init -q
GIT_CONFIG_GLOBAL="$T/gitconfig" GIT_CONFIG_SYSTEM=/dev/null \
  git -C "$T/repo" config user.name 'Net Trace Fixture'
GIT_CONFIG_GLOBAL="$T/gitconfig" GIT_CONFIG_SYSTEM=/dev/null \
  git -C "$T/repo" config user.email 'net-trace@example.invalid'
printf 'isolated fixture\n' >"$T/repo/README"
GIT_CONFIG_GLOBAL="$T/gitconfig" GIT_CONFIG_SYSTEM=/dev/null \
  git -C "$T/repo" add README
GIT_CONFIG_GLOBAL="$T/gitconfig" GIT_CONFIG_SYSTEM=/dev/null \
  GIT_AUTHOR_DATE='2000-01-01T00:00:00Z' GIT_COMMITTER_DATE='2000-01-01T00:00:00Z' \
  git -C "$T/repo" -c commit.gpgsign=false commit -q -m 'fixture'

env_vars=(
  "HOME=$T/home"
  "XDG_CONFIG_HOME=$T/cfg"
  "XDG_STATE_HOME=$T/state"
  "XDG_RUNTIME_DIR=$T/run"
  "XDG_CACHE_HOME=$T/cache"
  "XDG_DATA_HOME=$T/data"
  "GIT_CONFIG_GLOBAL=$T/gitconfig"
  "GIT_CONFIG_SYSTEM=/dev/null"
  "DBUS_SESSION_BUS_ADDRESS=unix:path=/dev/null/net-trace-no-dbus"
  "TERM=xterm-256color"
)
if ((!DAEMON)); then
  env_vars+=("THEGN_NO_DAEMON=1")
fi
if [[ -n $HOLD_MS ]]; then
  env_vars+=("THEGN_BENCH_RUN_MS=$HOLD_MS")
else
  env_vars+=("THEGN_BENCH_FIRST_FRAME_EXIT=1")
fi

mkdir -p "$OUT"
TRACE="$T/strace.out"
cmd=(strace -f -qq -ttt -s 4096 -o "$TRACE" -e trace=network env -i "PATH=$PATH" "${env_vars[@]}" "$BIN")
timeout_bin="$(pty_timeout_bin)"
if [[ -n $timeout_bin ]]; then
  cmd=("$timeout_bin" 120 "${cmd[@]}")
fi
printf -v command '%q ' ${cmd[@]+"${cmd[@]}"}
set +e
(
  cd "$T/repo"
  pty_run_to "$T/typescript" "$command"
)
run_status=$?
set -e

if [[ ! -s $TRACE ]]; then
  printf 'net-trace: trace invalid (missing or empty strace output)\n' >&2
  exit 2
fi
cp "$TRACE" "$OUT/strace.out"
window=first-frame
if [[ -n $HOLD_MS ]]; then
  window="hold-ms $HOLD_MS"
fi
set +e
python3 "$SCRIPT_DIR/first_run_net_trace.py" summarize "$TRACE" \
  --binary "$BIN_LABEL" --window "$window" --out "$OUT/net-trace.txt"
summary_status=$?
set -e

if ((DAEMON)); then
  stopped=0
  for _attempt in {1..30}; do
    set +e
    daemon_output="$(cd "$T/repo" && env -i "PATH=$PATH" "${env_vars[@]}" "$BIN" daemon stop 2>&1)"
    daemon_status=$?
    set -e
    if [[ $daemon_output == *'no thegn pane daemon is running'* ]]; then
      stopped=1
      break
    fi
    if ((daemon_status != 0)); then
      printf '%s\n' "$daemon_output" >&2
    fi
    sleep 0.1
  done
  if ((!stopped)); then
    printf 'net-trace: could not confirm isolated pane daemon shutdown\n' >&2
    exit 2
  fi
fi

if ((run_status != 0)); then
  printf 'net-trace: traced run failed to start or exit cleanly (status %s)\n' "$run_status" >&2
  exit 2
fi
exit "$summary_status"
