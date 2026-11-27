#!/usr/bin/env bash
set -euo pipefail
# Hermetic checks for setup-macos.sh's bootstrap authentication: the reviewed
# rustup pin, the pinned origin, and every refusal that must happen before the
# downloaded bytes execute. No network, no sudo, no macOS host required.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_ROOT="$(mktemp -d)"
chmod 700 "$TEST_ROOT"
trap 'rm -rf "$TEST_ROOT"' EXIT
STUB_BIN="$TEST_ROOT/bin"
mkdir -m 700 "$STUB_BIN"
TMPDIR="$TEST_ROOT/tmp"
mkdir -m 700 "$TMPDIR"
export TEST_ROOT STUB_BIN TMPDIR

# shellcheck source=test/lib/bootstrap-stubs.sh
# shellcheck disable=SC1091 # the checked-out library path is explicit
source "$ROOT/test/lib/bootstrap-stubs.sh"
bootstrap_stub_curl
bootstrap_stub_uname
PATH="$STUB_BIN:$PATH"
export PATH

mkdir -m 700 "$TEST_ROOT/home"
THEGN_SETUP_MACOS_SOURCE_ONLY=1
export THEGN_SETUP_MACOS_SOURCE_ONLY
source "$ROOT/setup-macos.sh"

failures=0
assert_case() {
  local name="$1"
  shift
  if "$@"; then
    printf 'ok %s\n' "$name"
  else
    printf 'FAIL %s\n' "$name" >&2
    failures=$((failures + 1))
  fi
}

# A rustup-init fixture that records how it was invoked. Every pin is set to the
# fixture's real size and digest, so only the refusal under test can stop it.
rustup_fixture_env() {
  TEST_ARCH="${1:-arm64}"
  TEST_CURL_FAIL=0
  export TEST_ARCH TEST_CURL_FAIL
  rm -f -- "$TEST_ROOT/rustup-ran"
  printf '#!/bin/sh\nprintf "%%s\\n" "$*" >"%s"\n' "$TEST_ROOT/rustup-ran" >"$TEST_ROOT/rustup-init"
  chmod 700 "$TEST_ROOT/rustup-init"
  TEST_PAYLOAD="$TEST_ROOT/rustup-init"
  TEST_EFFECTIVE_URL="https://static.rust-lang.org/rustup/archive/$RUSTUP_INIT_VERSION/aarch64-apple-darwin/rustup-init"
  export TEST_PAYLOAD TEST_EFFECTIVE_URL
  bootstrap_stub_curl
  local digest size
  digest="$(bootstrap_sha256_file "$TEST_ROOT/rustup-init")"
  size="$(wc -c <"$TEST_ROOT/rustup-init")"
  size="${size//[[:space:]]/}"
  RUSTUP_INIT_SHA256_AARCH64="$digest"
  RUSTUP_INIT_SIZE_AARCH64="$size"
  RUSTUP_INIT_SHA256_X86_64="$digest"
  RUSTUP_INIT_SIZE_X86_64="$size"
}
staging_is_clean() {
  [ -z "$(ls -A "$TMPDIR")" ]
}
# shellcheck disable=SC2016  # assertion matches shell source with literal variable names
case_rustup_pin_is_reviewed() {
  grep -Fq 'RUSTUP_INIT_VERSION="1.29.3"' "$ROOT/setup-macos.sh" &&
    grep -Fq 'RUSTUP_INIT_SHA256_AARCH64="63752a1919a3a4793173ec70f8f212621a7ce38ba6a246095984482f4de20f9f"' "$ROOT/setup-macos.sh" &&
    grep -Fq 'RUSTUP_INIT_SIZE_AARCH64=11408656' "$ROOT/setup-macos.sh" &&
    grep -Fq 'RUSTUP_INIT_SHA256_X86_64="bc107f1fb0228de7fa32413c89c1b305cea9cd256e94b01150cd054128145429"' "$ROOT/setup-macos.sh" &&
    grep -Fq 'RUSTUP_INIT_SIZE_X86_64=13041240' "$ROOT/setup-macos.sh" &&
    grep -Fq 'bootstrap_download_pinned "$url" "$sha256" "$size"' "$ROOT/setup-macos.sh" &&
    ! grep -Fq 'sh.rustup.rs | sh' "$ROOT/setup-macos.sh" &&
    ! grep -Fq '| sh -s --' "$ROOT/setup-macos.sh"
}
case_installs_only_authenticated_pinned_bytes() {
  rustup_fixture_env
  install_rustup_pinned || return 1
  [[ $(<"$TEST_ROOT/rustup-ran") == '-y --default-toolchain stable' ]] &&
    staging_is_clean
}
case_installs_the_intel_pin() {
  rustup_fixture_env x86_64
  install_rustup_pinned || return 1
  [[ -f $TEST_ROOT/rustup-ran ]] && staging_is_clean
}
case_refuses_wrong_digest_before_execution() {
  rustup_fixture_env
  RUSTUP_INIT_SHA256_AARCH64="$(printf '0%.0s' {1..64})"
  if install_rustup_pinned >"$TEST_ROOT/output" 2>&1; then return 1; fi
  [[ ! -e $TEST_ROOT/rustup-ran ]] &&
    grep -Fq 'SHA-256 mismatch' "$TEST_ROOT/output" &&
    staging_is_clean
}
case_refuses_wrong_size_before_execution() {
  rustup_fixture_env
  RUSTUP_INIT_SIZE_AARCH64="$((RUSTUP_INIT_SIZE_AARCH64 + 1))"
  if install_rustup_pinned >"$TEST_ROOT/output" 2>&1; then return 1; fi
  [[ ! -e $TEST_ROOT/rustup-ran ]] &&
    grep -Fq 'size mismatch' "$TEST_ROOT/output" &&
    staging_is_clean
}
case_refuses_unapproved_origin_before_execution() {
  rustup_fixture_env
  TEST_EFFECTIVE_URL="https://evil.invalid/rustup-init"
  export TEST_EFFECTIVE_URL
  if install_rustup_pinned >"$TEST_ROOT/output" 2>&1; then return 1; fi
  [[ ! -e $TEST_ROOT/rustup-ran ]] &&
    grep -Fq 'unapproved origin' "$TEST_ROOT/output" &&
    staging_is_clean
}
case_refuses_interrupted_transfer_before_execution() {
  rustup_fixture_env
  TEST_CURL_FAIL=1
  export TEST_CURL_FAIL
  if install_rustup_pinned >"$TEST_ROOT/output" 2>&1; then return 1; fi
  [[ ! -e $TEST_ROOT/rustup-ran ]] &&
    grep -Fq 'could not download' "$TEST_ROOT/output" &&
    staging_is_clean
}
case_refuses_unknown_architecture() {
  rustup_fixture_env powerpc
  if install_rustup_pinned >"$TEST_ROOT/output" 2>&1; then return 1; fi
  [[ ! -e $TEST_ROOT/rustup-ran ]] &&
    grep -Fq 'unsupported macOS architecture' "$TEST_ROOT/output"
}

assert_case rustup_bootstrap_uses_reviewed_version_size_and_digest case_rustup_pin_is_reviewed
assert_case installs_only_authenticated_pinned_bytes case_installs_only_authenticated_pinned_bytes
assert_case installs_the_intel_pin case_installs_the_intel_pin
assert_case refuses_wrong_digest_before_execution case_refuses_wrong_digest_before_execution
assert_case refuses_wrong_size_before_execution case_refuses_wrong_size_before_execution
assert_case refuses_unapproved_origin_before_execution case_refuses_unapproved_origin_before_execution
assert_case refuses_interrupted_transfer_before_execution case_refuses_interrupted_transfer_before_execution
assert_case refuses_unknown_architecture case_refuses_unknown_architecture

if ((failures)); then
  printf '%d bootstrap setup test(s) failed\n' "$failures" >&2
  exit 1
fi
printf 'all bootstrap setup tests passed\n'
