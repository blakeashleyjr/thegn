#!/usr/bin/env bash
# Hermetic checks for evidence classification and privacy; no macOS runtime calls.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=test/macos/lib.sh
# shellcheck disable=SC1091 # the checked-out library path is explicit
. "$root/test/macos/lib.sh"

fail() {
  echo "macos-ondevice-selftest: FAIL: $*" >&2
  exit 1
}

expect_contains() {
  case "$1" in *"$2"*) ;; *) fail "expected '$1' to contain '$2'" ;; esac
}

expect_contains "$(macos_ws05_network_verdict 0 0 connected expected 0 connected 0 denied 0 denied none)" "verified:"
expect_contains "$(macos_ws05_network_verdict 0 0 connected expected 0 connected 0 connected 0 denied none)" "not isolated:"
expect_contains "$(macos_ws05_network_verdict 0 0 connected expected 0 connected 124 timeout 0 timeout none)" "inconclusive:"
expect_contains "$(macos_ws05_network_verdict 125 125 missing none 125 missing 125 missing 125 missing none)" "inconclusive:"

export SP_REDACT_USER=alice SP_REDACT_HOST=alice-mac SP_REDACT_EXTRA_1=10.2.3.4 SP_REDACT_EXTRA_2=private/image
redacted="$(printf '%s' '/Users/alice on alice-mac 10.2.3.4 private/image' | sp_redact)"
case "$redacted" in
*alice* | *10.2.3.4* | *private/image*) fail "metadata redaction left a private value: $redacted" ;;
esac
expect_contains "$redacted" "<USER>"
expect_contains "$redacted" "<HOST>"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/thegn-macos-selftest.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
mkdir -m 700 "$tmp/WS05-P0-01"
printf 'preserve me\n' >"$tmp/WS05-P0-01.evidence.jsonl"
if bash -c '. "$1"; sc_init WS05-P0-01 "$2"' bash "$root/test/macos/lib.sh" "$tmp" >/dev/null 2>&1; then
  fail "evidence initializer overwrote an existing run"
fi
[ "$(cat "$tmp/WS05-P0-01.evidence.jsonl")" = "preserve me" ] || fail "existing evidence changed"

echo "macos-ondevice-selftest: ok (classification, redaction, collision safety)"
