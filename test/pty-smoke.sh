#!/usr/bin/env bash
# test/pty-smoke.sh — launch the interactive compositor inside a real PTY and
# require it to reach the first diff-flushed frame without panicking. This covers
# the termwiz/openpty path that ordinary non-PTY CLI smoke tests cannot touch.
set -euo pipefail

SZ="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/debug/thegn}"
SZ="$(cd "$(dirname "$SZ")" && pwd)/$(basename "$SZ")"
[[ -x $SZ ]] || {
  echo "not executable: $SZ (run: cargo build)" >&2
  exit 1
}

command -v script >/dev/null 2>&1 || {
  echo "skip PTY smoke: util-linux script(1) not found"
  exit 0
}
command -v timeout >/dev/null 2>&1 || {
  echo "skip PTY smoke: timeout(1) not found"
  exit 0
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
ok() { printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1"
  fail=1
}

run_case() {
  local name="$1" cols="$2" rows="$3"
  local root="$TMP/$name"
  local home="$root/home"
  local config="$root/config"
  local state="$root/state"
  local log="$root/typescript"
  mkdir -p "$home" "$config" "$state"

  local cmd
  printf -v cmd \
    'stty cols %q rows %q; env HOME=%q XDG_CONFIG_HOME=%q XDG_STATE_HOME=%q THEGN_BENCH_FIRST_FRAME_EXIT=1 %q' \
    "$cols" "$rows" "$home" "$config" "$state" "$SZ"

  if timeout 20s script -qec "$cmd" "$log" >/dev/null; then
    if grep -Eiq 'panicked at|thread .* panicked|fatal runtime error' "$log"; then
      bad "PTY launch $name (${cols}x${rows}) produced panic text"
      tail -80 "$log" || true
    else
      ok "PTY launch reaches first frame: $name (${cols}x${rows})"
    fi
  else
    bad "PTY launch exits non-zero or times out: $name (${cols}x${rows})"
    tail -80 "$log" || true
  fi
}

echo "thegn PTY smoke test"
run_case normal 100 30
run_case short 40 8

echo
if [[ $fail -eq 0 ]]; then
  printf '\033[32mall PTY smoke checks passed\033[0m\n'
else
  printf '\033[31mPTY smoke test FAILED\033[0m\n'
  exit 1
fi
