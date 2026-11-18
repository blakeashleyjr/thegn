#!/usr/bin/env bash
# WS03-P0-04 — copy-on-write options for ignored build dirs: native vs bwrap
# overlay vs reflink, on ONE filesystem. Run once per filesystem under test.
# Usage: CORPUS_RUST=/path/to/scratch/rust/workspace ws03-p0-04.sh OUTDIR WORKDIR [RUNS]
#   CORPUS_RUST  a small scratch Rust workspace (NOT the thegn checkout). Copied.
#   WORKDIR      an existing directory on the filesystem under test (btrfs, XFS,
#                ext4; a loop-mounted image is fine — say so in the record).
#   RUNS         incremental rebuilds per variant (default 3)
# Heavy: it runs cargo builds. Everything it creates lives under WORKDIR/tgspike-cow-<pid>
# and is removed on exit. Never point WORKDIR at a production state directory.
# The per-cache "verified on use" table cannot be scripted (corrupt-one-file
# experiments); this script emits the skeleton with sizes. See the procedure page.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS03-P0-04
sp_init "$item" "${1:-}"
workdir="${2:-}"
runs="${3:-3}"
[ "$(uname -s)" = Linux ] || sp_die "Linux only"
[ -d "$workdir" ] || sp_die "WORKDIR is not a directory"
sp_need_env CORPUS_RUST
[ -f "$CORPUS_RUST/Cargo.toml" ] || sp_die "CORPUS_RUST has no Cargo.toml"
sp_need cargo bwrap find awk

bwver="$(bwrap --version | awk '{print $2}')"
sp_kv bwrap_version "$bwver"
sp_kv bwrap_min_for_overlay "0.11"
sp_kv workdir_fs "$(stat -f -c %T "$workdir")"
sp_kv workdir_device "$(df -P "$workdir" | awk 'NR==2{print $1}')"

W="$workdir/tgspike-cow-$$"
mkdir -p "$W"
sp_on_exit "chmod -R u+w '$W' 2>/dev/null; rm -rf '$W'"
export CARGO_INCREMENTAL=1
src="$W/src"
cp -a "$CORPUS_RUST" "$src"
rm -rf "$src/target"
edit_file="$(find "$src" -name '*.rs' -not -path '*/target/*' | head -1)"
[ -n "$edit_file" ] || sp_die "no .rs file in the corpus"
cp "$edit_file" "$W/pristine.rs"

now_ms() { echo $(($(date +%s%N) / 1000000)); }
fresh_edit() { # reset the edited file and append a distinct edit
  cp "$W/pristine.rs" "$edit_file"
  printf '\n// tgspike edit %s\n' "$1" >>"$edit_file"
}
median() { sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'; }

# cold native build -> the pristine cold target, snapshotted as t-cold
t0=$(now_ms)
CARGO_TARGET_DIR="$W/t-cold" cargo build --manifest-path "$src/Cargo.toml" >"$SP_OUT/$item/cold.log" 2>&1
cold_ms=$(($(now_ms) - t0))
sp_kv cold_native_ms "$cold_ms"
sp_kv corpus_target_bytes "$(du -sb "$W/t-cold" | cut -f1)"

native_t=""
reflink_t=""
overlay_t=""
reflink_ok=1
overlay_ok=1
upper_ratio_max=0
hardlinks_native=""
hardlinks_overlay=""
inotify_overlay="not-checked"

for r in $(seq 1 "$runs"); do
  # --- native
  rm -rf "$W/t-native"
  cp -a --reflink=auto "$W/t-cold" "$W/t-native"
  fresh_edit "$r"
  touch "$W/marker"
  t0=$(now_ms)
  CARGO_TARGET_DIR="$W/t-native" cargo build --manifest-path "$src/Cargo.toml" >>"$SP_OUT/$item/native.log" 2>&1
  native_t="$native_t$(($(now_ms) - t0))
"
  changed="$(find "$W/t-native" -type f -newer "$W/marker" -printf '%s\n' | awk '{s+=$1} END{print s+0}')"
  hardlinks_native="$(find "$W/t-native" -type f -links +1 | wc -l | tr -d ' ')"

  # --- reflink (FICLONE via cp --reflink=always; fails on non-CoW filesystems)
  rm -rf "$W/t-reflink"
  if cp -a --reflink=always "$W/t-cold" "$W/t-reflink" 2>"$SP_OUT/$item/reflink.err"; then
    fresh_edit "$r"
    t0=$(now_ms)
    CARGO_TARGET_DIR="$W/t-reflink" cargo build --manifest-path "$src/Cargo.toml" >>"$SP_OUT/$item/reflink.log" 2>&1
    reflink_t="$reflink_t$(($(now_ms) - t0))
"
  else
    reflink_ok=0
  fi
  rm -rf "$W/t-reflink"

  # --- bwrap overlay: lower = t-cold (read-only), private upper + work on the same fs
  rm -rf "$W/upper" "$W/work" "$W/t-ovl"
  mkdir -p "$W/upper" "$W/work" "$W/t-ovl"
  fresh_edit "$r"
  # shellcheck disable=SC2016 # the single-quoted script is expanded by the inner sh on purpose
  inner='cargo build --manifest-path "$SRC/Cargo.toml"
         echo "HARDLINKS=$(find "$DEST" -type f -links +1 | wc -l)"
         if command -v inotifywait >/dev/null 2>&1; then
           inotifywait -qq -t 5 -e create -e modify "$DEST" & ip=$!
           sleep 0.5; touch "$DEST/.tgprobe"; if wait "$ip"; then echo INOTIFY=fired; else echo INOTIFY=silent; fi
         else echo INOTIFY=not-checked; fi'
  t0=$(now_ms)
  if env SRC="$src" DEST="$W/t-ovl" CARGO_TARGET_DIR="$W/t-ovl" \
    bwrap --bind / / --dev-bind /dev /dev --proc /proc \
    --overlay-src "$W/t-cold" --overlay "$W/upper" "$W/work" "$W/t-ovl" \
    -- sh -c "$inner" >"$SP_OUT/$item/overlay-$r.log" 2>&1; then
    overlay_t="$overlay_t$(($(now_ms) - t0))
"
    hardlinks_overlay="$(sed -n 's/^HARDLINKS=//p' "$SP_OUT/$item/overlay-$r.log" | tail -1)"
    inotify_overlay="$(sed -n 's/^INOTIFY=//p' "$SP_OUT/$item/overlay-$r.log" | tail -1)"
    upper_bytes="$(du -sb "$W/upper" | cut -f1)"
    sp_note "run $r: changed_bytes_native=$changed upper_bytes=$upper_bytes"
    ratio="$(awk -v u="$upper_bytes" -v c="$changed" 'BEGIN{ if(c<=0) print 999; else printf "%.2f", u/c }')"
    upper_ratio_max="$(awk -v a="$upper_ratio_max" -v b="$ratio" 'BEGIN{print (b>a)?b:a}')"
  else
    overlay_ok=0
    sp_note "run $r: overlay build FAILED (see overlay-$r.log): $(tail -n 3 "$SP_OUT/$item/overlay-$r.log" | tr '\n' ' ')"
  fi
  rm -rf "$W/upper" "$W/work" "$W/t-ovl"
  [ ! -e "$W/upper" ] && sp_kv "cleanup_run_$r" "uppers and work dirs removed"
done

# --- correctness corpus: cargo test, native vs overlay (same results required)
rm -rf "$W/t-native"
cp -a --reflink=auto "$W/t-cold" "$W/t-native"
fresh_edit final
nat_rc=0
CARGO_TARGET_DIR="$W/t-native" cargo test --manifest-path "$src/Cargo.toml" >"$SP_OUT/$item/test-native.log" 2>&1 || nat_rc=$?
mkdir -p "$W/upper" "$W/work" "$W/t-ovl"
ovl_rc=0
env CARGO_TARGET_DIR="$W/t-ovl" bwrap --bind / / --dev-bind /dev /dev --proc /proc \
  --overlay-src "$W/t-cold" --overlay "$W/upper" "$W/work" "$W/t-ovl" \
  -- cargo test --manifest-path "$src/Cargo.toml" >"$SP_OUT/$item/test-overlay.log" 2>&1 || ovl_rc=$?
sp_kv corpus_test_rc_native "$nat_rc"
sp_kv corpus_test_rc_overlay "$ovl_rc"
correctness_failures=0
[ "$nat_rc" = "$ovl_rc" ] || correctness_failures=1
[ "$overlay_ok" = 1 ] || correctness_failures=1

nat_med="$(printf '%s' "$native_t" | grep . | median)"
sp_kv incremental_native_ms_median "$nat_med"
sp_kv incremental_native_ms_all "$(printf '%s' "$native_t" | tr '\n' ' ')"
sp_kv incremental_reflink_ms_median "$([ "$reflink_ok" = 1 ] && printf '%s' "$reflink_t" | grep . | median || echo unsupported-on-this-fs)"
ovl_med=NA
[ "$overlay_ok" = 1 ] && ovl_med="$(printf '%s' "$overlay_t" | grep . | median)"
sp_kv incremental_overlay_ms_median "$ovl_med"
sp_kv hardlinked_files_native "$hardlinks_native"
sp_kv hardlinked_files_overlay "${hardlinks_overlay:-NA}"
sp_kv inotify_on_overlay "$inotify_overlay"
sp_kv upper_to_changed_ratio_max "$upper_ratio_max"

# --- cache skeleton (the 16 auto_cache_mounts() entries; fill kind / verified / handling by hand)
sp_note ""
sp_note "## cache classification skeleton (re-read auto_cache_mounts() at run time; fill by hand)"
sp_note "path | exists | bytes | store kind | verified on use | recommended handling"
for p in .cargo/registry .cargo/git .rustup .npm .cache/pnpm .cache/yarn go/pkg/mod .cache/go-build .cache/pip .cache/uv .m2/repository .gradle/caches .cache/sccache .cache/prek .cache/pre-commit .cache/nix; do
  if [ -d "$HOME/$p" ]; then
    sp_note "$p | yes | $(du -sb "$HOME/$p" 2>/dev/null | cut -f1) | ? | ? | ?"
  else
    sp_note "$p | no | - | ? | ? | ?"
  fi
done

if [ "$overlay_ok" = 1 ] && [ "$nat_med" -gt 0 ]; then
  verdict="$(awk -v o="$ovl_med" -v n="$nat_med" -v u="$upper_ratio_max" -v c="$correctness_failures" \
    'BEGIN{ r=o/n; ok=(r<=1.3 && c==0 && u<=2.0); printf "incremental_ratio=%.2f (<=1.3) correctness_failures=%d (==0) upper_ratio=%.2f (<=2.0) => this filesystem %s", r, c, u, ok?"PASSES":"FAILS" }')"
else
  verdict="overlay build failed on this filesystem => FAILS (see overlay-*.log)"
fi
sp_verdict "adopt overlay only if incremental <= 1.3x native, zero corpus correctness failures, upper <= 2x changed bytes; otherwise private per-worktree dirs (reflink numbers recorded, no wt new fast path)" \
  "$(stat -f -c %T "$workdir"): $verdict"
