#!/usr/bin/env bash
# Install a published thegn release into clean podman containers and smoke-test it.
#
# Downloads the release tarballs (x86_64 gnu and musl) and their .sha256 files
# with `gh release download`, verifies the checksums, then for each container
# image installs `thegn` into ~/.local/bin and runs `thegn --version`,
# `thegn doctor`, `thegn wt new smoke` (in a fresh git repo) and `thegn wt list`.
# Prints one PASS/FAIL line per image.
#
# Usage: test/clean-install-check.sh [--tag TAG]
#        test/clean-install-check.sh --self-test
#   --tag TAG    release tag to install (default: the latest published release)
#   --self-test  exercise the checksum verifier on fixtures; downloads nothing
# Env: PODMAN (default podman), GH (default gh). Needs gh, podman and sha256sum.
# Not wired into `just test` or CI: it pulls images and needs network access.

set -euo pipefail

PODMAN="${PODMAN:-podman}"
GH="${GH:-gh}"
GNU_TARGET=x86_64-unknown-linux-gnu
MUSL_TARGET=x86_64-unknown-linux-musl

# Image, release target, label. The gnu tarball runs on the glibc images; the
# static musl tarball is the one for ubuntu:20.04 (oldest glibc in the set).
IMAGES=(
  "ubuntu:24.04 $GNU_TARGET gnu"
  "debian:12 $GNU_TARGET gnu"
  "fedora:41 $GNU_TARGET gnu"
  "ubuntu:20.04 $MUSL_TARGET musl"
)

# Runs inside each container as root with the release directory mounted at
# /dl (read-only). Written to a file in the private work dir, never the download dir.
read -r -d '' INNER_SCRIPT <<'EOF' || true
set -eu
step() {
  cur="$1"
  shift
  echo "== $cur"
  "$@" || { echo "step failed: $cur"; exit 1; }
}
export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
  apt-get update -qq >/dev/null
  apt-get install -y -qq git tar ca-certificates >/dev/null
elif command -v dnf >/dev/null 2>&1; then
  dnf install -y -q git tar >/dev/null
fi
stage="$(mktemp -d)"
tar -xzf "/dl/$TARBALL" -C "$stage"
mkdir -p "$HOME/.local/bin"
# The host looks for thegn-musl beside its own executable, so ship both.
install -m 0755 "$stage/thegn" "$stage/thegn-musl" "$HOME/.local/bin/"
export PATH="$HOME/.local/bin:$PATH"
step version thegn --version
step doctor thegn doctor
repo="$(mktemp -d)/repo"
git init -q "$repo"
git -C "$repo" -c user.name=clean-install -c user.email=clean-install@invalid \
  commit -q --allow-empty -m init
cd "$repo"
step wt-new thegn wt new smoke
step wt-list thegn wt list
echo "clean-install: all steps passed"
EOF

usage() {
  sed -n '/^# Usage:/,/^# Env:/p' "$0" | sed 's/^# \{0,1\}//'
}

# verify_checksum DIR ARCHIVE: succeeds only when DIR/ARCHIVE.sha256 records the
# SHA-256 of DIR/ARCHIVE, in the `HASH  NAME` format the release workflow writes.
verify_checksum() {
  local dir="$1" archive="$2" sumfile recorded named extra fresh
  sumfile="$dir/${archive%.tar.gz}.sha256"
  [ -f "$dir/$archive" ] && [ -f "$sumfile" ] || return 1
  read -r recorded named extra <"$sumfile" || true
  [[ $recorded =~ ^[a-f0-9]{64}$ ]] || return 1
  if [ -n "${named:-}" ] && [ "$named" != "$archive" ]; then
    return 1
  fi
  [ -z "${extra:-}" ] || return 1
  fresh="$(sha256sum "$dir/$archive" | awk '{print $1}')"
  [ "$recorded" = "$fresh" ]
}

self_test() {
  local tmp rc=0 digest
  tmp="$(mktemp -d)"
  printf 'payload\n' >"$tmp/a.tar.gz"
  digest="$(sha256sum "$tmp/a.tar.gz" | awk '{print $1}')"

  printf '%s  a.tar.gz\n' "$digest" >"$tmp/a.sha256"
  verify_checksum "$tmp" a.tar.gz || {
    echo "self-test: good checksum rejected" >&2
    rc=1
  }

  printf '%064d  a.tar.gz\n' 0 >"$tmp/a.sha256"
  verify_checksum "$tmp" a.tar.gz && {
    echo "self-test: wrong digest accepted" >&2
    rc=1
  }

  printf 'zz  a.tar.gz\n' >"$tmp/a.sha256"
  verify_checksum "$tmp" a.tar.gz && {
    echo "self-test: malformed digest accepted" >&2
    rc=1
  }

  printf '%s  other.tar.gz\n' "$digest" >"$tmp/a.sha256"
  verify_checksum "$tmp" a.tar.gz && {
    echo "self-test: foreign file name accepted" >&2
    rc=1
  }

  printf '%s  a.tar.gz\n' "$digest" >"$tmp/a.sha256"
  rm -f "$tmp/a.tar.gz"
  verify_checksum "$tmp" a.tar.gz && {
    echo "self-test: missing archive accepted" >&2
    rc=1
  }

  rm -rf "$tmp"
  if [ "$rc" -eq 0 ]; then
    echo "self-test: ok"
  fi
  return "$rc"
}

main() {
  local tag="" arg archive target label image dl work logdir
  local failures=0 log status step_failed kept

  while [ $# -gt 0 ]; do
    arg="$1"
    case "$arg" in
    --tag)
      [ $# -ge 2 ] || {
        echo "clean-install-check: --tag needs a value" >&2
        exit 2
      }
      tag="$2"
      shift 2
      ;;
    --tag=*)
      tag="${arg#--tag=}"
      shift
      ;;
    --self-test)
      self_test
      exit $?
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "clean-install-check: unknown argument: $arg" >&2
      usage >&2
      exit 2
      ;;
    esac
  done

  for tool in "$GH" "$PODMAN" sha256sum awk; do
    command -v "$tool" >/dev/null 2>&1 || {
      echo "clean-install-check: required tool not found: $tool" >&2
      exit 2
    }
  done
  if [ -n "$tag" ] && ! [[ $tag =~ ^[A-Za-z0-9._+-]+$ ]]; then
    echo "clean-install-check: invalid tag: $tag" >&2
    exit 2
  fi

  if [ -z "$tag" ]; then
    tag="$("$GH" release view --json tagName --jq .tagName)"
  fi
  echo "clean-install-check: release $tag"

  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  dl="$work/release"
  logdir="$work/logs"
  mkdir -p "$dl" "$logdir"
  printf '%s\n' "$INNER_SCRIPT" >"$work/inner.sh"

  for target in "$GNU_TARGET" "$MUSL_TARGET"; do
    archive="thegn-$tag-$target.tar.gz"
    "$GH" release download "$tag" --dir "$dl" \
      --pattern "$archive" --pattern "${archive%.tar.gz}.sha256"
    verify_checksum "$dl" "$archive" || {
      echo "clean-install-check: checksum verification failed for $archive" >&2
      exit 1
    }
  done
  echo "clean-install-check: checksums verified"

  for entry in "${IMAGES[@]}"; do
    read -r image target label <<<"$entry"
    archive="thegn-$tag-$target.tar.gz"
    log="$logdir/${image//[^A-Za-z0-9]/_}.log"
    status=0
    "$PODMAN" run --rm -v "$dl:/dl:ro" -v "$work/inner.sh:/run/inner.sh:ro" \
      -e "TARBALL=$archive" "$image" bash /run/inner.sh >"$log" 2>&1 || status=$?
    if [ "$status" -eq 0 ]; then
      echo "PASS $image ($label)"
    else
      failures=$((failures + 1))
      step_failed="$(grep -o 'step failed: .*' "$log" | tail -n 1 || true)"
      echo "FAIL $image ($label): ${step_failed:-exit $status} (log: $log)"
      # The work dir is deleted on exit; keep the log for the user.
      kept="${TMPDIR:-/tmp}/clean-install-$tag-${image//[^A-Za-z0-9]/_}.log"
      cp "$log" "$kept"
      echo "  log kept at $kept"
    fi
  done

  if [ "$failures" -gt 0 ]; then
    echo "clean-install-check: $failures of ${#IMAGES[@]} images failed"
    exit 1
  fi
  echo "clean-install-check: all ${#IMAGES[@]} images passed"
}

main "$@"
