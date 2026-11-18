#!/usr/bin/env bash
# WS10-P2-04 — plugin stream framing: base64-in-NDJSON vs raw pipe, Rust and
# Python echo children. Usage: ws10-p2-04.sh OUTDIR
# Builds the standalone probe crate (deps: serde_json, base64 from crates.io)
# from a COPY under OUTDIR; runs about a minute of load. Quiet machine, please.
# macOS numbers (advisory) come from running this script on the Mac.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS10-P2-04
sp_init "$item" "${1:-}"
sp_need cargo python3 cat
work="$SP_OUT/$item/crate"
rm -rf "$work"
cp -R "$here/probes/plugin-framing" "$work"
rm -rf "$work/target" "$work/Cargo.lock"
export CARGO_TARGET_DIR="$SP_OUT/$item/target"
sp_on_exit "rm -rf '$CARGO_TARGET_DIR'"
cargo build --release --manifest-path "$work/Cargo.toml" >"$SP_OUT/$item/build.log" 2>&1 ||
  sp_die "probe build failed, see $SP_OUT/$item/build.log"
bin="$CARGO_TARGET_DIR/release/plugin-framing"

sp_kv rustc "$(rustc -V)"
sp_kv python "$(python3 --version 2>&1)"
sp_kv cpu "$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo 2>/dev/null | head -1 || sysctl -n machdep.cpu.brand_string 2>/dev/null)"
sp_kv cores "$(nproc 2>/dev/null || sysctl -n hw.ncpu)"
sp_kv governor "$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo unknown)"

res="$SP_OUT/$item/results.txt"
{
  "$bin" bench raw raw cat
  "$bin" bench rust framed "$bin" echo-child
  "$bin" bench python framed python3 "$work/bench_echo.py"
} >"$res"
sp_note ""
sp_note "## results (RESULT lines: latency in us, mbps = MB/s round-tripped, 32 KiB frames for size>=32768)"
cat "$res" >>"$SP_EVID"

get() { # get LABEL SIZE FIELD
  sed -n "s/^RESULT label=$1 mode=[a-z]* size=$2 .*$3=\([0-9.]*\).*/\1/p" "$res" | head -1
}
raw99="$(get raw 1 lat_p99_us)"
rust99="$(get rust 1 lat_p99_us)"
py99="$(get python 1 lat_p99_us)"
rust_mbps="$(get rust 32768 mbps)"
py_mbps="$(get python 32768 mbps)"
sp_kv added_p99_us_rust_1B "$((rust99 - raw99))"
sp_kv added_p99_us_python_1B "$((py99 - raw99))"
sp_kv rust_mbps_32KiB "$rust_mbps"
sp_kv python_mbps_32KiB "$py_mbps"
sp_kv targets "rust >= 100 MB/s, python >= 20 MB/s, added 1-byte echo p99 <= 2000 us"

if awk -v m="$py_mbps" -v a="$((py99 - raw99))" -v b="$((rust99 - raw99))" 'BEGIN{exit !(m<20 || a>2000 || b>2000)}'; then
  v="open a 0.8 item for stream:fd (python MB/s ${py_mbps} vs 20, added p99 python $((py99 - raw99)) us / rust $((rust99 - raw99)) us vs 2000)"
else
  v="base64 only: close (python ${py_mbps} MB/s >= 20, added p99 python $((py99 - raw99)) us / rust $((rust99 - raw99)) us <= 2000)"
fi
sp_verdict "Python below 20 MB/s, or added 1-byte p99 above 2 ms => open a 0.8 item for stream:fd; otherwise base64 only" "$v"
