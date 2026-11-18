#!/usr/bin/env bash
# WS09-P0-04 — choose the mux credit window; measure ControlMaster vs mux and
# optimistic OPEN. Usage: ws09-p0-04.sh OUTDIR [RUNS] [TARGET]
#   RUNS    repetitions per configuration (default 3)
#   TARGET  ssh destination with key login, default localhost
# Builds the standalone probe crate from a COPY under OUTDIR (std only, no network
# for deps). Each run is ~(idle 3 s + flood 6 s). Needs `ssh TARGET true` to work
# non-interactively (BatchMode). Quiet machine; numbers are advisory (B9).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS09-P0-04
sp_init "$item" "${1:-}"
runs="${2:-3}"
target="${3:-localhost}"
sp_need cargo ssh awk
ssh -o BatchMode=yes -o ConnectTimeout=5 "$target" true || sp_die "ssh $target must work with key login and no prompts"

work="$SP_OUT/$item/crate"
rm -rf "$work"
cp -R "$here/probes/mux-bench" "$work"
rm -rf "$work/target" "$work/Cargo.lock"
export CARGO_TARGET_DIR="$SP_OUT/$item/target"
sp_on_exit "rm -rf '$CARGO_TARGET_DIR'"
cargo build --release --manifest-path "$work/Cargo.toml" >"$SP_OUT/$item/build.log" 2>&1 ||
  sp_die "probe build failed, see $SP_OUT/$item/build.log"
bin="$CARGO_TARGET_DIR/release/mux-bench"

sp_kv ssh_version "$(ssh -V 2>&1)"
sp_kv cpu "$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo 2>/dev/null | head -1)"
sp_kv kernel "$(uname -r)"
sp_kv target "$target"

res="$SP_OUT/$item/results.txt"
: >"$res"
for r in $(seq 1 "$runs"); do
  for win in 65536 262144 1048576; do
    sp_say "run $r window $win"
    "$bin" run mux "$target" "$bin" "$win" 3 6 >>"$res"
  done
  sp_say "run $r controlmaster baseline"
  "$bin" run cm "$target" "$bin" 0 3 6 >>"$res"
done
sp_say "unary / optimistic OPEN"
"$bin" unary "$target" "$bin" 500 >>"$res"
sp_note ""
sp_note "## raw results (microseconds; mbps = flood throughput)"
cat "$res" >>"$SP_EVID"
sp_kv window_unit "bytes: 65536 = 64 KiB, 262144 = 256 KiB, 1048576 = 1024 KiB"

# mean of a field over the runs of one (mode, window)
mean() { # mean MODE WINDOW FIELD
  awk -v m="$1" -v w="$2" -v f="$3" '
    $2=="mode="m && $3=="window="w { for(i=1;i<=NF;i++){ split($i,kv,"="); if(kv[1]==f){s+=kv[2];n++} } }
    END{ if(n) printf "%.0f", s/n; else print "NA" }' "$res"
}

best=""
summary=""
for win in 65536 262144 1048576; do
  i99="$(mean mux "$win" idle_p99_us)"
  f99="$(mean mux "$win" flood_p99_us)"
  mb="$(mean mux "$win" flood_mbps)"
  ok=no
  if awk -v i="$i99" -v f="$f99" 'BEGIN{exit !(i>0 && f<=2*i)}'; then
    ok=yes
    [ -n "$best" ] || best="$win"
  fi
  sp_kv "window_$win" "idle_p99=${i99}us flood_p99=${f99}us flood=${mb}MB/s passes(flood_p99<=2x idle_p99)=$ok"
  summary="$summary $win:$ok"
done
sp_kv cm_baseline "idle_p99=$(mean cm n/a idle_p99_us)us flood_p99=$(mean cm n/a flood_p99_us)us flood=$(mean cm n/a flood_mbps)MB/s"
opt0="$(sed -n 's/^UNARY optimistic=0 p50_us=\([0-9]*\).*/\1/p' "$res")"
opt1="$(sed -n 's/^UNARY optimistic=1 p50_us=\([0-9]*\).*/\1/p' "$res")"
sp_kv optimistic_open_p50 "wait-for-OPEN_OK=${opt0}us DATA-before-OPEN_OK=${opt1}us (delta=$((opt0 - opt1))us)"

if [ -n "$best" ]; then
  v="window = $best bytes (smallest passing). per-window:$summary"
else
  v="no window passes: keep 256 KiB default, record the failed criterion, escalate to the lead (mux only when chosen explicitly, never by auto). per-window:$summary"
fi
sp_verdict "echo p99 under flood <= 2x idle p99; choose the smallest passing window of 64/256/1024 KiB; none passes => keep 256 KiB and escalate" "$v"
