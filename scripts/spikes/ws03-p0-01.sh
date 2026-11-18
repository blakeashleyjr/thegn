#!/usr/bin/env bash
# WS03-P0-01 — confirm landlock 0.4.7 / seccompiler 0.5.0 APIs on an ABI 10 host.
# Usage: ws03-p0-01.sh OUTDIR
# Builds the standalone probe crate (scripts/spikes/probes/confine-apis) from a
# COPY under OUTDIR, one cargo feature per symbol, runs every case in a forked
# child, and writes OUTDIR/WS03-P0-01.evidence.txt. Runs cargo (a small build):
# do it on a quiet machine, once. Procedure:
# docs/superpowers/specs/sandbox-remote-spikes/WS03-P0-01.md
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS03-P0-01
sp_init "$item" "${1:-}"
[ "$(uname -s)" = Linux ] || sp_die "Linux only (Landlock, seccomp)"
sp_need cargo rustc

work="$SP_OUT/$item/crate"
rm -rf "$work"
cp -R "$here/probes/confine-apis" "$work"
rm -rf "$work/target" "$work/Cargo.lock"
export CARGO_TARGET_DIR="$SP_OUT/$item/target"
sp_on_exit "rm -rf '$CARGO_TARGET_DIR'"

repo_msrv="$(sed -n 's/^rust-version *= *"\(.*\)"/\1/p' "$here/../../Cargo.toml" | head -1)"
sp_kv workspace_msrv "${repo_msrv:-unknown}"
sp_kv kernel "$(uname -r)"
sp_kv rustc "$(rustc -V)"

manifest="$work/Cargo.toml"
bin="$CARGO_TARGET_DIR/release/confine-apis-spike"

build() { # build FEATURES -> 0 if it compiled
  local f="$1" rc=0
  local -a fa=()
  [ -z "$f" ] || fa=(--features "$f")
  rm -f "$bin"
  cargo build --release --manifest-path "$manifest" ${fa[@]+"${fa[@]}"} >"$SP_OUT/$item/build-${f:-base}.log" 2>&1 || rc=$?
  return "$rc"
}

declare_result() { sp_note "RESULT $1: $2"; }

# 0. base build (both crates, no optional symbols)
if build ""; then
  declare_result base "compiles"
  sp_cap host-abi 10 "$bin" abi
  abi="$(sed -n 's/host_landlock_abi=//p' "$SP_OUT/$item/host-abi.out")"
  sp_kv host_landlock_abi "${abi:-unknown}"
  [ "${abi:-0}" -ge 10 ] 2>/dev/null || sp_note "WARNING: host ABI < 10: UDP/ResolveUnix results are not representative"
  sp_cap ll-fs-hard 20 "$bin" landlock fs-v6-hard
  sp_cap ll-fs-best 20 "$bin" landlock fs-v6-best
  for k in dword masked qword; do
    sp_cap "seccomp-$k" 20 "$bin" seccomp "$k"
    sp_cap "dump-$k" 20 "$bin" dump "$k"
  done
  sp_cap syscalls 10 "$bin" syscalls
else
  declare_result base "DOES NOT COMPILE (see build-base.log) — verify the pins resolve before reading further"
  tail -n 25 "$SP_OUT/$item/build-base.log" >>"$SP_EVID"
  exit 1
fi

# 1. one feature per symbol: compile failure == candidate "no" (triage the error!)
for f in udp resolve_unix scope_abstract scope_signal libc_open_tree_attr; do
  if build "$f"; then
    declare_result "$f" "compiles"
    case "$f" in
    libc_open_tree_attr) sp_cap "run-$f" 10 "$bin" syscalls ;;
    *) sp_cap "run-$f" 20 "$bin" landlock "$f" ;;
    esac
  else
    declare_result "$f" "COMPILE ERROR (candidate 'no'; confirm the error is a missing/renamed symbol, not a probe typo)"
    tail -n 15 "$SP_OUT/$item/build-$f.log" >>"$SP_EVID"
  fi
done

# 2. open_tree_attr numbers from the UAPI headers
for h in /usr/include/asm-generic/unistd.h /usr/include/x86_64-linux-gnu/asm/unistd_64.h /usr/include/asm/unistd_64.h; do
  [ -f "$h" ] && sp_cap "uapi-$(basename "$h")" 10 grep -n "open_tree_attr" "$h"
done

# 3. declared rust-version of both crates vs the workspace MSRV
for c in landlock-0.4.7 seccompiler-0.5.0; do
  m="$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" -maxdepth 2 -type d -name "$c" 2>/dev/null | head -1)"
  if [ -n "$m" ] && [ -f "$m/Cargo.toml" ]; then
    sp_kv "msrv_$c" "$(sed -n 's/^rust-version *= *"\(.*\)"/\1/p' "$m/Cargo.toml" | head -1)"
  else
    sp_kv "msrv_$c" "crate source not found under cargo registry"
  fi
done
if [ -n "${repo_msrv:-}" ] && sp_have rustup && rustup toolchain list 2>/dev/null | grep -q "^$repo_msrv"; then
  if cargo "+$repo_msrv" build --release --manifest-path "$manifest" >"$SP_OUT/$item/build-msrv.log" 2>&1; then
    sp_kv msrv_build "builds on $repo_msrv"
  else
    sp_kv msrv_build "FAILS on $repo_msrv (see build-msrv.log)"
  fi
else
  sp_kv msrv_build "toolchain $repo_msrv not installed: run 'rustup toolchain install $repo_msrv' and re-run, or record the pin"
fi

# 4. verdict: the seccomp bypass question is computable; the rest is the table in the page
dw="$(grep -c 'call=high_bits.*BLOCKED' "$SP_OUT/$item/seccomp-dword.out" || true)"
mk="$(grep -c 'call=high_bits.*BLOCKED' "$SP_OUT/$item/seccomp-masked.out" || true)"
qw="$(grep -c 'call=high_bits.*BLOCKED' "$SP_OUT/$item/seccomp-qword.out" || true)"
sp_verdict "Dword/MaskedEq must block TIOCSTI with high 32 bits set (else P1-04 hand-writes the compare); every symbol is 'no' unless a test result shows 'yes'" \
  "dword_blocks_high_bits=$dw masked_blocks_high_bits=$mk qword_blocks_high_bits=$qw (qword=0 expected: shows the bypass). Fill the rest of the table from RESULT lines above."
