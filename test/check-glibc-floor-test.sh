#!/usr/bin/env bash
# Self-test for scripts/check-glibc-floor.sh using a stub objdump.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
: >"$tmp/fake-binary"
cat >"$tmp/bin/objdump" <<'STUB'
#!/usr/bin/env bash
for v in $GLIBC_STUB_VERSIONS; do
  printf '0000 g DF .text 0000 GLIBC_%s memcpy\n' "$v"
done
STUB
chmod +x "$tmp/bin/objdump"
export PATH="$tmp/bin:$PATH"

GLIBC_STUB_VERSIONS="2.2.5 2.17 2.28" "$ROOT/scripts/check-glibc-floor.sh" "$tmp/fake-binary" 2.28 >/dev/null
if GLIBC_STUB_VERSIONS="2.2.5 2.38" "$ROOT/scripts/check-glibc-floor.sh" "$tmp/fake-binary" 2.28 2>/dev/null; then
  echo "expected GLIBC_2.38 to fail the 2.28 floor" >&2
  exit 1
fi
# 2.4 must sort below 2.28 numerically, not lexically beyond it.
GLIBC_STUB_VERSIONS="2.4" "$ROOT/scripts/check-glibc-floor.sh" "$tmp/fake-binary" 2.28 >/dev/null
echo "check-glibc-floor-test: ok"
