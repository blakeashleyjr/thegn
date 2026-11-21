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

checkout_resolved=1
if checkout_abs=$(cd -P "$checkout" 2>/dev/null && pwd -P); then
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
    depinfo_lib=
    foreign_source=0
    listed_source_count=0
    depinfo_source_identities=()
    depinfo_data=
    if ! depinfo_data=$(cat "$core_depinfo" 2>/dev/null); then
      depinfo_supported=0
    else
      depinfo_supported=1
    fi
    # Rustc's make-style dep-info escapes whitespace and line continuations.
    # This diagnostic intentionally refuses those path forms instead of
    # splitting a pathname and accidentally counting its prefix as coverage.
    case "$depinfo_data" in
      *\\*|*$'\t'*|*$'\r'* ) depinfo_supported=0 ;;
    esac

    # Compare source identities using the physical parent directory on both
    # sides. macOS can expose one checkout through spellings such as /tmp and
    # /private/tmp, and dep-info preserves whichever spelling Cargo observed.
    source_identity() {
      local source_path source_dir source_base source_dir_abs
      source_path=$1
      if [[ $source_path != /* ]]; then
        source_path=$checkout/$source_path
      fi
      case "$source_path" in *[[:space:]]* ) return 1 ;; esac
      source_dir=${source_path%/*}
      source_base=${source_path##*/}
      if source_dir_abs=$(cd -P "$source_dir" 2>/dev/null && pwd -P); then
        printf '%s/%s\n' "$source_dir_abs" "$source_base"
      else
        return 1
      fi
    }

    set -f
    for depinfo_token in $depinfo_data; do
      depinfo_token=${depinfo_token%:}
      case "$depinfo_token" in
      */crates/thegn-core/src/*.rs | crates/thegn-core/src/*.rs)
        listed_source_count=$((listed_source_count + 1))
        if ((checkout_resolved == 0)) || ! token_identity=$(source_identity "$depinfo_token"); then
          depinfo_supported=0
          continue
        fi
        depinfo_source_identities+=("$token_identity")
        case "$token_identity" in
        "$checkout"/crates/thegn-core/src/*.rs) ;;
        *) foreign_source=1 ;;
        esac
        [[ -f $token_identity ]] || foreign_source=1
        if [[ $depinfo_token == */crates/thegn-core/src/lib.rs || $depinfo_token == crates/thegn-core/src/lib.rs ]]; then
          depinfo_lib=$depinfo_token
        fi
        ;;
      esac
    done
    set +f

    missing_sources=unknown
    core_src_dir=$checkout/crates/thegn-core/src
    if ((checkout_resolved == 0)) || [[ ! -d $core_src_dir ]]; then
      depinfo_supported=0
    fi
    if ((depinfo_supported)) && ((checkout_resolved)) && [[ -d $core_src_dir ]]; then
      comparison_dir=$(mktemp -d "${TMPDIR:-/tmp}/cargo-provenance.XXXXXX" 2>/dev/null) || comparison_dir=
      if [[ -z $comparison_dir ]]; then
        depinfo_supported=0
      else
        trap 'rm -rf "$comparison_dir"' EXIT HUP INT TERM
      fi
    fi
    if ((depinfo_supported)) && ((checkout_resolved)) && [[ -d $core_src_dir && -n ${comparison_dir:-} ]]; then
      required_source_identities=()
      if ! find "$core_src_dir" -type f -name '*.rs' -print0 >"$comparison_dir/required.nul" 2>/dev/null; then
        depinfo_supported=0
      else
        required_source_count=0
        while IFS= read -r -d '' source_file; do
          if ! source_identity_resolved=$(source_identity "$source_file"); then
            depinfo_supported=0
            break
          fi
          required_source_identities+=("$source_identity_resolved")
          required_source_count=$((required_source_count + 1))
        done <"$comparison_dir/required.nul"
        ((required_source_count > 0)) || depinfo_supported=0
      fi

      # Normalize each side once, then compare the sets in one awk pass. In
      # particular, do not resolve source paths from inside a nested pair loop:
      # real crates contain hundreds of source files.
      if ((depinfo_supported)); then
        if ! {
          if ((${#depinfo_source_identities[@]})); then
            for identity in "${depinfo_source_identities[@]}"; do
              printf 'D\t%s\n' "$identity"
            done
          fi
          if ((${#required_source_identities[@]})); then
            for identity in "${required_source_identities[@]}"; do
              printf 'R\t%s\n' "$identity"
            done
          fi
        } >"$comparison_dir/records"; then
          depinfo_supported=0
        elif ! missing_sources=$(awk 'substr($0, 1, 2) == "D\t" { listed[substr($0, 3)] = 1; next }
            substr($0, 1, 2) == "R\t" && !listed[substr($0, 3)] { missing++ }
            END { print missing + 0 }' "$comparison_dir/records" 2>/dev/null); then
          depinfo_supported=0
        elif [[ ! $missing_sources =~ ^[0-9]+$ ]]; then
          depinfo_supported=0
        fi
      fi
    fi
    if ((depinfo_supported == 0)); then
      missing_sources=unknown
      foreign_source=1
      printf '%s\n' 'cargo-provenance: refusing unsupported dep-info paths or unresolved/failed source comparison' >&2
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
