#!/usr/bin/env bash
# WS03-P0-02 — exec cost of a self-exec helper vs a small static binary.
# Usage: THEGN_SELF_EXEC_BIN=/path/to/release/thegn ws03-p0-02.sh OUTDIR [RUNS]
#   THEGN_SELF_EXEC_BIN  release thegn built from the THROWAWAY branch whose
#                        `__sandbox-exec -- <argv>` early dispatch execs <argv>
#                        (patch described in the procedure page; not in the repo).
#   RUNS                 interleaved runs per subject (default 300, minimum 200)
# Builds the standalone probe crate from a copy under OUTDIR (small, std-only).
# Run it on every machine (x86_64 dev host AND an aarch64 machine); each run
# writes its own evidence file and prints that machine's verdict.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS03-P0-02
sp_init "$item" "${1:-}"
runs="${2:-300}"
[ "$runs" -ge 200 ] || sp_die "RUNS must be at least 200 (issue: >= 200 interleaved runs)"
[ "$(uname -s)" = Linux ] || sp_die "Linux only"
sp_need cargo sha256sum
sp_need_env THEGN_SELF_EXEC_BIN
[ -x "$THEGN_SELF_EXEC_BIN" ] || sp_die "THEGN_SELF_EXEC_BIN is not an executable file"
/bin/true || sp_die "/bin/true missing"

work="$SP_OUT/$item/crate"
rm -rf "$work"
cp -R "$here/probes/exec-cost" "$work"
rm -rf "$work/target" "$work/Cargo.lock"
export CARGO_TARGET_DIR="$SP_OUT/$item/target"
sp_on_exit "rm -rf '$CARGO_TARGET_DIR'"

arch="$(uname -m)"
tgt=""
if sp_have rustup && rustup target list --installed 2>/dev/null | grep -q "^$arch-unknown-linux-musl$"; then
  tgt="$arch-unknown-linux-musl"
  cargo build --release --manifest-path "$work/Cargo.toml" --target "$tgt" >"$SP_OUT/$item/build.log" 2>&1
  bindir="$CARGO_TARGET_DIR/$tgt/release"
else
  sp_note "NOTE: musl target not installed; building a crt-static glibc binary instead (record this in the decision)"
  RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --manifest-path "$work/Cargo.toml" \
    --target "$arch-unknown-linux-gnu" >"$SP_OUT/$item/build.log" 2>&1
  bindir="$CARGO_TARGET_DIR/$arch-unknown-linux-gnu/release"
fi
static_bin="$bindir/confine-exec-static"
bench="$bindir/exec-bench"
[ -x "$static_bin" ] && [ -x "$bench" ] || sp_die "probe build failed, see $SP_OUT/$item/build.log"

sp_kv arch "$arch"
sp_kv kernel "$(uname -r)"
sp_kv machine "$(cat /sys/devices/virtual/dmi/id/product_name 2>/dev/null || echo unknown)"
sp_kv cpu_model "$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -1)"
sp_kv governor "$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo unknown)"
sp_kv thegn_image_bytes "$(stat -c %s "$THEGN_SELF_EXEC_BIN")"
sp_kv static_image_bytes "$(stat -c %s "$static_bin")"
sp_kv runs "$runs"
sp_note "Tip: for a clean run set the governor to performance, or record it as above."

true_bin="$(command -v true)"
"$bench" "$runs" 20 \
  "baseline=$true_bin" \
  "selfexec=$THEGN_SELF_EXEC_BIN __sandbox-exec -- $true_bin" \
  "static=$static_bin __sandbox-exec -- $true_bin" >"$SP_OUT/$item/bench.out"
sp_note ""
sp_note "## bench (microseconds, interleaved)"
cat "$SP_OUT/$item/bench.out" >>"$SP_EVID"

# SHA-256 timing of each image (the once-per-process materialisation hash), best of 5.
hash_ms() {
  local best=999999999 t0 t1 d
  for _ in 1 2 3 4 5; do
    t0=$(date +%s%N)
    sha256sum "$1" >/dev/null
    t1=$(date +%s%N)
    d=$(((t1 - t0) / 1000))
    [ "$d" -lt "$best" ] && best=$d
  done
  echo "$best"
}
sp_kv sha256_thegn_us_best_of_5 "$(hash_ms "$THEGN_SELF_EXEC_BIN")"
sp_kv sha256_static_us_best_of_5 "$(hash_ms "$static_bin")"

get() { sed -n "s/^RESULT label=$1 .*$2=\([0-9]*\).*/\1/p" "$SP_OUT/$item/bench.out" | head -1; }
b95="$(get baseline p95_us)"
s95="$(get selfexec p95_us)"
t95="$(get static p95_us)"
b50="$(get baseline p50_us)"
s50="$(get selfexec p50_us)"
t50="$(get static p50_us)"
o95=$((s95 - b95))
o50=$((s50 - b50))
sp_kv selfexec_overhead_p50_us "$o50"
sp_kv selfexec_overhead_p95_us "$o95"
sp_kv static_overhead_p50_us "$((t50 - b50))"
sp_kv static_overhead_p95_us "$((t95 - b95))"
sp_note "budget (plan 3.6, whole helper): p50 <= 4000us, p95 <= 10000us over /bin/true; this spike's trigger is the bare exec p95 > 8000us"
if [ "$o95" -gt 8000 ]; then
  res="ship [[bin]] thegn-confine-exec on $arch (self-exec p95 overhead ${o95}us > 8000us)"
else
  res="self-exec acceptable on $arch (p95 overhead ${o95}us <= 8000us). The decision needs BOTH machines: any one over 8000us means the small binary."
fi
sp_verdict "self-exec p95 overhead over /bin/true above 8 ms on either machine => ship [[bin]] thegn-confine-exec; otherwise self-exec" "$res"
