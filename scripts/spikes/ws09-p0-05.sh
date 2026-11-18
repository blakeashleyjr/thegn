#!/usr/bin/env bash
# WS09-P0-05 — size/compress/decompress/delta table for the full musl thegn vs a
# feature-trimmed thegn-remote. It MEASURES binaries you already built (the builds
# are heavy and the feature split is a throwaway source edit; see the procedure page).
# Usage: FULL_BIN=... TRIMMED_BIN=... ws09-p0-05.sh OUTDIR
#   FULL_BIN          musl thegn (expected ~82,779,192 bytes; re-measure)
#   TRIMMED_BIN       throwaway-split thegn-remote
#   PREV_FULL_BIN     previous alpha build of FULL_BIN   (optional, for --patch-from)
#   PREV_TRIMMED_BIN  previous alpha build of TRIMMED_BIN (optional)
#   SMOKE_PASS=1      you ran the step-4 smoke subset on TRIMMED_BIN and it passed
# Decompression time is only meaningful on the 2-vCPU VM of the procedure; the
# script records nproc so a run elsewhere is visibly not comparable.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS09-P0-05
sp_init "$item" "${1:-}"
sp_need gzip zstd sha256sum
sp_need_env FULL_BIN TRIMMED_BIN
[ -f "$FULL_BIN" ] && [ -f "$TRIMMED_BIN" ] || sp_die "FULL_BIN / TRIMMED_BIN must be files"

tmp="$SP_OUT/$item/work"
mkdir -p "$tmp"
sp_on_exit "rm -rf '$tmp'"
sp_kv nproc "$(nproc 2>/dev/null || sysctl -n hw.ncpu)"
sp_kv zstd_version "$(zstd --version | head -n1)"
sp_kv full_sha256 "$(sha256sum "$FULL_BIN" | awk '{print $1}')"
sp_kv trimmed_sha256 "$(sha256sum "$TRIMMED_BIN" | awk '{print $1}')"

size() { stat -c %s "$1" 2>/dev/null || stat -f %z "$1"; }
now_us() { echo $(($(date +%s%N) / 1000)); }
best_us() { # best_us N cmd... : best-of-N wall time in microseconds, stdout discarded
  local n="$1" best=999999999999 t0 d
  shift
  for _ in $(seq 1 "$n"); do
    t0=$(now_us)
    "$@" >/dev/null
    d=$(($(now_us) - t0))
    [ "$d" -lt "$best" ] && best=$d
  done
  echo "$best"
}

measure() { # measure LABEL BIN PREV
  local label="$1" bin="$2" prev="${3:-}" raw gz zs
  raw="$(size "$bin")"
  gzip -9 -c "$bin" >"$tmp/$label.gz"
  zstd -q -19 --long=27 -c "$bin" >"$tmp/$label.zst"
  gz="$(size "$tmp/$label.gz")"
  zs="$(size "$tmp/$label.zst")"
  sp_kv "${label}_raw_bytes" "$raw"
  sp_kv "${label}_gzip9_bytes" "$gz"
  sp_kv "${label}_zstd19_long27_bytes" "$zs"
  sp_kv "${label}_zstd_decompress_us_best5" "$(best_us 5 zstd -dc --long=27 "$tmp/$label.zst")"
  sp_kv "${label}_gzip_decompress_us_best5" "$(best_us 5 gzip -dc "$tmp/$label.gz")"
  if [ -n "$prev" ] && [ -f "$prev" ]; then
    zstd -q -19 --long=27 --patch-from="$prev" -c "$bin" >"$tmp/$label.patch"
    sp_kv "${label}_patch_from_bytes" "$(size "$tmp/$label.patch")"
    sp_kv "${label}_patch_pct_of_zst" "$(awk -v p="$(size "$tmp/$label.patch")" -v z="$zs" 'BEGIN{printf "%.1f", 100*p/z}')"
  else
    sp_kv "${label}_patch_from_bytes" "not measured (no PREV_*_BIN)"
  fi
}

measure full "$FULL_BIN" "${PREV_FULL_BIN:-}"
measure trimmed "$TRIMMED_BIN" "${PREV_TRIMMED_BIN:-}"

full_raw="$(size "$FULL_BIN")"
trimmed_raw="$(size "$TRIMMED_BIN")"
pct="$(awk -v t="$trimmed_raw" -v f="$full_raw" 'BEGIN{printf "%.1f", 100*t/f}')"
sp_kv trimmed_pct_of_full_raw "$pct"
smoke="NOT RECORDED"
[ "${SMOKE_PASS:-0}" = 1 ] && smoke=passed
sp_kv smoke_subset "$smoke"
if awk -v p="$pct" 'BEGIN{exit !(p<=40.0)}' && [ "$smoke" = passed ]; then
  res="ship thegn-remote (raw ${pct}% <= 40% and smoke passed)"
else
  res="ship the full musl thegn (raw ${pct}% vs 40% threshold; smoke: $smoke)"
fi
sp_verdict "ship thegn-remote iff raw <= 40% of musl thegn AND the smoke subset passes; delivery always compressed; median patch-from <= 20% of .zst => record a delta follow-up" "$res"
