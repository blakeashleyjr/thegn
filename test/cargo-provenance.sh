#!/usr/bin/env bash
# Opt-in, read-only THE-609 diagnostic: show which toolchain, wrapper, and cached
# artifact a build used, and whether its dep-info names this checkout. This is not
# part of `just ci`; Python 3 is required when a dep-info file is present.
set -u

script_dir=${BASH_SOURCE[0]%/*}
if [[ $script_dir == "${BASH_SOURCE[0]}" ]]; then
  script_dir=.
fi

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
  if checkout=$(git rev-parse --show-toplevel 2>/dev/null && printf '\001'); then
    checkout=${checkout%$'\001'}
    checkout=${checkout%$'\n'}
  else
    checkout=
  fi
  if [[ -z $checkout ]]; then
    checkout=$PWD
  fi
fi

checkout_resolved=1
if checkout_abs=$(CDPATH= cd -P "$checkout" >/dev/null 2>&1 && pwd -P && printf '\001'); then
  checkout_abs=${checkout_abs%$'\001'}
  checkout_abs=${checkout_abs%$'\n'}
  checkout=$checkout_abs
else
  checkout_resolved=0
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
    missing_sources=unknown
    depinfo_supported=0
    foreign_source=0
    depinfo_lib=
    parser_result=
    if ((checkout_resolved)) && command -v python3 >/dev/null 2>&1 && [[ ! $checkout =~ [[:space:]] ]]; then
      if parser_result=$(python3 "$script_dir/cargo-provenance-parse.py" "$checkout" "$core_depinfo" 2>/dev/null); then
        IFS=$'\t' read -r missing_sources foreign_source depinfo_lib <<<"$parser_result"
        if [[ $missing_sources =~ ^[0-9]+$ && ($foreign_source == 0 || $foreign_source == 1) && -n $depinfo_lib ]]; then
          depinfo_supported=1
        fi
      fi
    fi
    if ((depinfo_supported == 0)); then
      missing_sources=unknown
      foreign_source=1
      printf '%s\n' 'cargo-provenance: refusing unsupported dep-info paths, unavailable Python 3, or unresolved source comparison' >&2
    fi
    printf 'provenance.core_depinfo_missing_sources: %s\n' "$missing_sources"
    if [[ -n $depinfo_lib ]]; then
      printf 'provenance.core_depinfo_lib: %s\n' "$depinfo_lib"
      if ((foreign_source == 0 && missing_sources == 0)); then
        match=yes
      else
        match=no
      fi
    else
      printf '%s\n' 'provenance.core_depinfo_lib: none'
      if ((depinfo_supported == 0)); then
        match=no
      else
        match=unknown
      fi
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
