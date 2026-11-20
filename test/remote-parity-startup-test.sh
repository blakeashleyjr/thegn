#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$ROOT/test/remote-parity.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin"

cat >"$TMP/bin/thegn" <<'STUB'
#!/usr/bin/env bash
printf 'thegn %s\n' "$*" >>"$STUB_LOG"
exit 97
STUB
cat >"$TMP/bin/curl" <<'STUB'
#!/usr/bin/env bash
printf 'curl %s\n' "$*" >>"$STUB_LOG"
exit 97
STUB
chmod +x "$TMP/bin/thegn" "$TMP/bin/curl"
: >"$TMP/calls.log"

plan="$(THEGN_BIN="$TMP/bin/thegn" bash "$SCRIPT")"
grep -Fq -- "--startup-only" <<<"$plan"
grep -Fq -- "--fill-spare" <<<"$plan"
grep -Fq -- "--tui-probe" <<<"$plan"

set +e
THEGN_BIN="$TMP/bin/thegn" STUB_LOG="$TMP/calls.log" PATH="$TMP/bin:$PATH" \
  bash "$SCRIPT" --startup-only >"$TMP/refused.out" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ]
grep -Fq "refusing: missing --env --i-accept-spend" "$TMP/refused.out"
[ ! -s "$TMP/calls.log" ]

echo "remote-parity-startup-test: PASS"
