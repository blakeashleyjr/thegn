#!/usr/bin/env bash
# Opt-in, read-only THE-609 diagnostic: show which toolchain, wrapper, and cached
# artifact a build used, and whether its dep-info names this checkout. This is not
# part of `just ci`.
set -u

usage() {
  printf '%s\n' 'Usage: test/cargo-provenance.sh [--target-dir DIR] [--checkout DIR] [--strict] [-h|--help]' >&2
}

target_dir_arg=
checkout_arg=
strict=0
target_dir_set=0
checkout_set=0

while (($# > 0)); do
  case "$1" in
  --target-dir)
    if (($# < 2)) || [[ -z $2 || $2 == -* ]]; then
      usage
      exit 2
    fi
    target_dir_arg=$2
    target_dir_set=1
    shift 2
    ;;
  --checkout)
    if (($# < 2)) || [[ -z $2 || $2 == -* ]]; then
      usage
      exit 2
    fi
    checkout_arg=$2
    checkout_set=1
    shift 2
    ;;
  --strict)
    strict=1
    shift
    ;;
  -h | --help)
    printf '%s\n' 'Usage: test/cargo-provenance.sh [--target-dir DIR] [--checkout DIR] [--strict] [-h|--help]'
    exit 0
    ;;
  *)
    usage
    exit 2
    ;;
  esac
done

if ((checkout_set)); then
  checkout=$checkout_arg
else
  checkout=$(git rev-parse --show-toplevel 2>/dev/null)
  if [[ -z $checkout ]]; then
    checkout=$PWD
  fi
fi

checkout_given=$checkout
if [[ $checkout == /* ]]; then
  checkout_abs=$(cd "$checkout" 2>/dev/null && pwd -P)
else
  checkout_abs=$(cd "$PWD/$checkout" 2>/dev/null && pwd -P)
fi
if [[ -n $checkout_abs ]]; then
  checkout=$checkout_abs
elif [[ $checkout != /* ]]; then
  checkout=$PWD/$checkout
fi

if ((target_dir_set)); then
  target_dir=$target_dir_arg
elif [[ -n ${CARGO_TARGET_DIR:-} ]]; then
  target_dir=$CARGO_TARGET_DIR
else
  target_dir=$checkout/target
fi
if [[ $target_dir != /* ]]; then
  target_dir=$PWD/$target_dir
fi

head_oid=unknown
if command -v git >/dev/null 2>&1; then
  if resolved_head=$(git -C "$checkout" rev-parse HEAD 2>/dev/null); then
    head_oid=$resolved_head
  fi
fi

if [[ ${CARGO_TARGET_DIR+x} ]]; then
  cargo_target_dir_env=$CARGO_TARGET_DIR
else
  cargo_target_dir_env='unset'
fi

rustc_wrapper=${RUSTC_WRAPPER:-${CARGO_BUILD_RUSTC_WRAPPER:-none}}
rustflags=${RUSTFLAGS:-unset}

printf 'provenance.checkout: %s\n' "$checkout"
printf 'provenance.head: %s\n' "$head_oid"
printf 'provenance.target_dir: %s\n' "$target_dir"
printf 'provenance.cargo_target_dir_env: %s\n' "$cargo_target_dir_env"
printf 'provenance.rustc_wrapper: %s\n' "$rustc_wrapper"
printf 'provenance.rustflags: %s\n' "$rustflags"

if command -v rustc >/dev/null 2>&1; then
  if rustc_vv=$(rustc -vV 2>/dev/null); then
    printf '%s\n' "$rustc_vv" | awk '{ printf "provenance.rustc_vv.%d: %s\n", NR, $0 }'
  else
    printf '%s\n' 'provenance.rustc_vv: unavailable'
  fi
else
  printf '%s\n' 'provenance.rustc_vv: unavailable'
fi

if command -v sccache >/dev/null 2>&1; then
  if sccache_stats=$(sccache --show-stats 2>/dev/null); then
    if [[ -n $sccache_stats ]]; then
      printf '%s\n' "$sccache_stats" | awk 'NR <= 40 { printf "provenance.sccache.%d: %s\n", NR, $0 }'
    else
      printf '%s\n' 'provenance.sccache: stats-unavailable'
    fi
  else
    printf '%s\n' 'provenance.sccache: stats-unavailable'
  fi
else
  printf '%s\n' 'provenance.sccache: absent'
fi

deps_dir=$target_dir/debug/deps
printf 'provenance.deps_dir: %s\n' "$deps_dir"

if [[ ! -d $deps_dir ]]; then
  printf '%s\n' 'provenance.core_artifact: none'
  printf '%s\n' 'provenance.core_depinfo: none'
  printf '%s\n' 'provenance.core_depinfo_missing_sources: 0'
  printf '%s\n' 'provenance.core_depinfo_matches_checkout: unknown'
  verdict=unknown
else
  core_artifact=$(find "$deps_dir" -maxdepth 1 -type f -name 'libthegn_core-*.rlib' -exec ls -t {} + 2>/dev/null | head -n 1)
  if [[ -n $core_artifact ]]; then
    printf 'provenance.core_artifact: %s\n' "$core_artifact"
    if command -v sha256sum >/dev/null 2>&1; then
      artifact_hash=$(sha256sum "$core_artifact" 2>/dev/null)
      artifact_hash=${artifact_hash%% *}
    elif command -v shasum >/dev/null 2>&1; then
      artifact_hash=$(shasum -a 256 "$core_artifact" 2>/dev/null)
      artifact_hash=${artifact_hash%% *}
    else
      artifact_hash=unavailable
    fi
    if [[ -z $artifact_hash ]]; then
      artifact_hash=unavailable
    fi
    printf 'provenance.core_artifact_sha256: %s\n' "$artifact_hash"
    if artifact_mtime=$(date -r "$core_artifact" +%s 2>/dev/null); then
      printf 'provenance.core_artifact_mtime: %s\n' "$artifact_mtime"
    else
      printf '%s\n' 'provenance.core_artifact_mtime: unavailable'
    fi
  else
    printf '%s\n' 'provenance.core_artifact: none'
    printf '%s\n' 'provenance.core_artifact_sha256: unavailable'
    printf '%s\n' 'provenance.core_artifact_mtime: unavailable'
  fi

  core_depinfo=$(find "$deps_dir" -maxdepth 1 -type f -name 'thegn_core-*.d' -exec ls -t {} + 2>/dev/null | head -n 1)
  if [[ -n $core_depinfo ]]; then
    printf 'provenance.core_depinfo: %s\n' "$core_depinfo"
    depinfo_lib=
    foreign_source=0
    listed_source_count=0
    depinfo_tokens=$(tr ' ' '\n' <"$core_depinfo")
    for depinfo_token in $depinfo_tokens; do
      depinfo_token=${depinfo_token%:}
      case "$depinfo_token" in
      */crates/thegn-core/src/*.rs | crates/thegn-core/src/*.rs)
        listed_source_count=$((listed_source_count + 1))
        if [[ $depinfo_token == /* ]]; then
          token_dir=$(dirname "$depinfo_token")
          token_base=$(basename "$depinfo_token")
          if token_dir_abs=$(cd "$token_dir" 2>/dev/null && pwd -P); then
            token_abs=$token_dir_abs/$token_base
          else
            token_abs=$depinfo_token
          fi
          case "$token_abs" in
          "$checkout"/crates/thegn-core/src/*.rs | "$checkout_given"/crates/thegn-core/src/*.rs) ;;
          *) foreign_source=1 ;;
          esac
          [[ -f $token_abs ]] || foreign_source=1
        else
          [[ -f $checkout/$depinfo_token ]] || foreign_source=1
        fi
        if [[ $depinfo_token == */crates/thegn-core/src/lib.rs || $depinfo_token == crates/thegn-core/src/lib.rs ]]; then
          depinfo_lib=$depinfo_token
        fi
        ;;
      esac
    done

    missing_sources=0
    core_src_dir=$checkout/crates/thegn-core/src
    if [[ -d $core_src_dir ]]; then
      while IFS= read -r source_file; do
        source_listed=0
        for depinfo_token in $depinfo_tokens; do
          depinfo_token=${depinfo_token%:}
          if [[ $depinfo_token == "$source_file" || $depinfo_token == "${source_file#"$checkout"/}" ]]; then
            source_listed=1
            break
          fi
        done
        if ((source_listed == 0)); then
          missing_sources=$((missing_sources + 1))
        fi
      done < <(find "$core_src_dir" -type f -name '*.rs' 2>/dev/null)
    fi
    printf 'provenance.core_depinfo_missing_sources: %s\n' "$missing_sources"
    if [[ -n $depinfo_lib ]]; then
      printf 'provenance.core_depinfo_lib: %s\n' "$depinfo_lib"
      if ((foreign_source == 0 && missing_sources == 0 && listed_source_count > 0)); then
        match=yes
      else
        match=no
      fi
    else
      printf '%s\n' 'provenance.core_depinfo_lib: none'
      match=unknown
    fi
  else
    printf '%s\n' 'provenance.core_depinfo: none'
    printf '%s\n' 'provenance.core_depinfo_lib: none'
    printf '%s\n' 'provenance.core_depinfo_missing_sources: 0'
    match=unknown
  fi

  printf 'provenance.core_depinfo_matches_checkout: %s\n' "$match"
  case "$match" in
  yes) verdict=ok ;;
  no) verdict=foreign-depinfo ;;
  *) verdict=unknown ;;
  esac
fi

printf 'provenance.verdict: %s\n' "$verdict"
if ((strict)) && [[ $verdict == foreign-depinfo ]]; then
  exit 1
fi
