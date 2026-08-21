#!/usr/bin/env bash
# Portable PTY wrapping for the launch / smoke / perf harnesses.
#
# termwiz refuses to start without a terminal, so every harness that drives or
# times a real thegn launch has to run it under a PTY — and the obvious tool,
# `script`, has two mutually incompatible CLIs:
#
#   util-linux (Linux):  script -qec 'CMD' TYPESCRIPT
#   BSD (macOS):         script -q TYPESCRIPT CMD ARGS...     # no -e, no -c
#
# A util-linux invocation on macOS dies with "illegal option -- e", which is how
# `just bench` and the PTY harnesses failed there. Source this and use:
#
#   source "$REPO/test/lib/pty.sh"
#   pty_run_to "$log" "env FOO=1 target/debug/thegn"   # capture the typescript
#   pty_run "target/debug/thegn"                       # discard it
#   hyperfine "$(pty_cmd "target/release/thegn")"      # hyperfine runs sh -c
#
# One caveat worth knowing: BSD `script` reports the exit status of `script`
# itself, not of the wrapped command, so callers that need the inner status must
# assert on the typescript's contents (which the PTY harnesses already do).
#
# `timeout` has the same shape of problem — GNU coreutils, absent from a stock
# macOS — so `pty_timeout_bin` resolves it (or its `gtimeout` alias) and prints
# nothing when there is none.

# True when `script` is the util-linux flavour (`-qec CMD FILE`) rather than BSD.
_pty_is_util_linux() {
  script --version 2>/dev/null | grep -qi util-linux
}

# Print a shell command line that runs "$1" under a PTY, discarding the
# typescript. For consumers like hyperfine that want a string, not execution.
pty_cmd() {
  if _pty_is_util_linux; then
    printf "script -qec '%s' /dev/null" "$1"
  else
    printf "script -q /dev/null /bin/sh -c '%s'" "$1"
  fi
}

# Run "$2" (a shell command line) under a PTY, writing the typescript to "$1".
pty_run_to() {
  local log="$1" cmd="$2"
  if _pty_is_util_linux; then
    script -qec "$cmd" "$log"
  else
    script -q "$log" /bin/sh -c "$cmd"
  fi
}

# Run "$1" under a PTY, discarding the typescript.
pty_run() {
  pty_run_to /dev/null "$1"
}

# Print the name of a usable `timeout` binary, or nothing when there is none.
# Stock macOS has neither; with Homebrew/nix coreutils it is `gtimeout`.
pty_timeout_bin() {
  if command -v timeout >/dev/null 2>&1; then
    echo timeout
  elif command -v gtimeout >/dev/null 2>&1; then
    echo gtimeout
  fi
}
