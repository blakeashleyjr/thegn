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

python3 - "$root/test/macos/apple-container-list-probe.py" <<'PY'
import importlib.util
import sys

spec = importlib.util.spec_from_file_location("apple_list_probe", sys.argv[1])
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)

marker = "mixedCaseMarker"
fixtures = [
    (
        '[{"Name":"/thegn-run-label","Config":{"Labels":{"thegn.spike":"mixedCaseMarker"}}}]',
        (True, True, True),
    ),
    (
        '{"containers":[{"names":["thegn-run-label"],"labels":["thegn.spike=mixedCaseMarker"]}]}',
        (True, True, True),
    ),
    (
        '[{"Name":"thegn-run-label-extra","Labels":{"thegn.spike":"mixedCaseMarker"}}]',
        (True, False, False),
    ),
    (
        '[{"Name":"thegn-run-label","Labels":{"thegn.spike":"other"}},'
        '{"Name":"another-container","Labels":{"thegn.spike":"mixedCaseMarker"}}]',
        (True, True, False),
    ),
    ("not json", (False, False, False)),
]
for payload, expected in fixtures:
    actual = probe.proof_flags(payload, "thegn-run-label", marker)
    if actual != expected:
        raise SystemExit("apple list parser fixture mismatch: %r != %r" % (actual, expected))
PY

resource_id="$(printf '%s' 's05xlAQhI' | macos_ws05_resource_id)"
[ "$resource_id" = s05xlaqhi ] || fail "mixed-case resource suffix was not normalized: $resource_id"
case "$resource_id" in *[!a-z0-9]* | "") fail "resource id is not portable lowercase: $resource_id" ;; esac

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
