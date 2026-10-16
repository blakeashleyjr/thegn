#!/usr/bin/env bash
# Linker wrapper for x86_64-pc-windows-gnu, used ONLY by `just test-windows-wine`.
#
# Why it exists: iroh-relay (via iroh -> thegn-svc/thegn-host) declares
# `crate-type = ["lib", "cdylib"]`, and cargo builds a dependency's cdylib even
# though nothing links it. When rustc builds an rlib and a cdylib in one
# session it compiles the shared objects at the Rust export level, so they
# carry a dllexport directive for every Rust symbol (~68k in a debug build).
# PE export ordinals are 16-bit, and GNU ld fails with "export ordinal too
# large". Built as a cdylib alone the same crate exports nothing and links.
#
# The fix is narrow: a DLL link that fails with exactly that error is replaced
# by an empty stub DLL at the same path. Nothing loads that DLL (it is a
# by-product of the dependency's crate-type list, not an input to any test
# binary), so a stub is behaviourally identical for the test run. Every other
# link, and every other failure, is passed through untouched. The real linker
# comes from THEGN_MINGW_CC (the flake's cross cc, via the recipe).
set -euo pipefail

real="${THEGN_MINGW_CC:?THEGN_MINGW_CC must name the mingw-w64 cc}"

out=""
implib=()
prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then
    out="$a"
  fi
  case "$a" in
  -Wl,--out-implib=*) implib=("$a") ;;
  esac
  prev="$a"
done

case "$out" in
*.dll) ;;
*) exec "$real" "$@" ;;
esac

log="$(mktemp)"
trap 'rm -f "$log"' EXIT
if "$real" "$@" 2>"$log"; then
  cat "$log" >&2
  exit 0
fi
if grep -q "export ordinal too large" "$log"; then
  echo "mingw-link: $out exceeds the PE export table; writing an empty stub DLL" >&2
  stub_src="$(mktemp --suffix=.c)"
  trap 'rm -f "$log" "$stub_src"' EXIT
  printf 'int thegn_wine_stub_dll;\n' >"$stub_src"
  # -nostdlib: the stub needs no CRT (and the cross cc's default thread
  # library is not on its search path). The import library rustc asked for
  # is still produced, since rustc checks that it exists.
  "$real" -shared -nostdlib -Wl,--entry=0 "${implib[@]}" -o "$out" "$stub_src"
  exit 0
fi
cat "$log" >&2
exit 1
