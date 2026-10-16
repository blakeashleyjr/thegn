#!/usr/bin/env bash
# Cargo target runner for x86_64-pc-windows-gnu: run one PE test binary under
# Wine, headless, and exit with its status.
#
# Wired by `just test-windows-wine` through
# CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER, so cargo / nextest invoke it as
#   wine-runner.sh <path/to/test.exe> [libtest args...]
# for the listing pass and for every test process.
#
# What it does, and why:
#   - A PRIVATE prefix (never ~/.wine): a test binary may write the registry,
#     %APPDATA% and the prefix's C: drive, and those must not leak into a
#     prefix the developer uses for anything else. It lives under
#     $XDG_CACHE_HOME so `cargo clean` does not force a ~10 s re-init, and it is
#     keyed by the wine store path, so a wine bump gets a fresh prefix instead
#     of an "outdated prefix" upgrade on the first test.
#   - The prefix is initialised ONCE, under flock: nextest starts many test
#     processes at the same moment, and concurrent first-run `wineboot`s on one
#     prefix corrupt it.
#   - Headless: DISPLAY/WAYLAND_DISPLAY are dropped so nothing opens a window,
#     and mscoree/mshtml are disabled so wineboot never tries to install Mono or
#     Gecko (they are GUI-only, need a download, and no test uses them).
#   - THEGN_UNDER_WINE=1 is exported so a test that genuinely cannot work
#     under Wine can see it — but the preferred skip mechanism is the
#     `wine` nextest profile in .config/nextest.toml, which needs no Rust edit.
set -euo pipefail

#
# Two recipe-only modes (never passed by cargo, whose first argument is always
# the .exe path): `--print-prefix` prints the prefix this runner uses, and
# `--init-only` creates it and exits, so `just test-windows-wine` can start one
# persistent wineserver against the same prefix before the first test runs.
if [ "$#" -lt 1 ]; then
  echo "usage: wine-runner.sh <test.exe> [args...] | --init-only | --print-prefix" >&2
  exit 2
fi

wine_bin="${THEGN_WINE:-wine}"
if ! command -v "$wine_bin" >/dev/null 2>&1; then
  echo "wine-runner: '$wine_bin' not found; run via 'just test-windows-wine'," >&2
  echo "  which builds the flake-pinned wine (.#wine) and sets THEGN_WINE." >&2
  exit 127
fi

# Key the prefix by the resolved wine install so a wine upgrade never reuses
# a prefix created by a different version.
wine_path="$(command -v "$wine_bin")"
wine_real="$(readlink -f "$wine_path")"
wine_key="$(printf '%s' "$wine_real" | cksum | cut -d' ' -f1)"
cache_root="${XDG_CACHE_HOME:-$HOME/.cache}/thegn"
export WINEPREFIX="${THEGN_WINEPREFIX:-$cache_root/wine-prefix-$wine_key}"
export WINEDEBUG="${WINEDEBUG:--all}"
export WINEDLLOVERRIDES="${WINEDLLOVERRIDES:-mscoree,mshtml=}"
export THEGN_UNDER_WINE=1
unset DISPLAY WAYLAND_DISPLAY

if [ "$1" = "--print-prefix" ]; then
  printf '%s\n' "$WINEPREFIX"
  exit 0
fi

if [ ! -f "$WINEPREFIX/system.reg" ]; then
  mkdir -p "$cache_root"
  (
    flock 9
    # Re-check under the lock: another test process may have finished it.
    if [ ! -f "$WINEPREFIX/system.reg" ]; then
      echo "wine-runner: initialising prefix $WINEPREFIX (once)" >&2
      "$wine_bin" wineboot --init >/dev/null 2>&1
      # wineboot returns before wineserver has flushed the registry; wait for
      # it so the next process sees a complete prefix.
      wineserver_bin="$(dirname "$wine_real")/wineserver"
      if [ -x "$wineserver_bin" ]; then
        "$wineserver_bin" --wait
      fi
    fi
  ) 9>"$cache_root/wine-prefix.lock"
fi

if [ "$1" = "--init-only" ]; then
  exit 0
fi

# Under nextest (which captures each test's output through pipes), a cold
# wineserver makes THIS process the parent of Wine's resident services, which
# inherit the pipes and hold them open: nextest then hangs waiting for EOF.
# `just test-windows-wine` pre-boots the services and sets THEGN_WINE_WARM.
if [ -n "${NEXTEST_RUN_ID:-}" ] && [ -z "${THEGN_WINE_WARM:-}" ]; then
  echo "wine-runner: running under nextest without a warm wineserver; this can" >&2
  echo "  hang. Use 'just test-windows-wine', which pre-boots one." >&2
fi

# exec: the test binary's exit status IS this script's exit status, which is
# what cargo and nextest read pass/fail from.
exec "$wine_bin" "$@"
