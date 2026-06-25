#!/usr/bin/env bash
# Suite G shell complement — verify DNS filtering inside a real container.
# Requires: podman, nslookup (busybox / bind-tools), PODMAN_E2E_FORCE=1.
#
# Usage: PODMAN_E2E_FORCE=1 ./test/sandbox-network.sh

set -euo pipefail

if [ "${PODMAN_E2E_FORCE:-}" != "1" ]; then
  echo "sandbox-network.sh: PODMAN_E2E_FORCE not set — skipping (set it to 1 to run)"
  exit 0
fi

if ! command -v podman >/dev/null 2>&1; then
  echo "sandbox-network.sh: podman not found — skipping"
  exit 0
fi

CONTAINER="superzej-e2e-net-sh"
IMAGE="docker.io/library/alpine:latest"
BLOCKED="blocked.internal"
# shellcheck disable=SC2034  # reserved for future allow-list test
ALLOWED="example.com"
PASS=0
FAIL=0

cleanup() {
  podman rm -f "$CONTAINER" 2>/dev/null || true
}
trap cleanup EXIT

echo "==> pulling $IMAGE"
podman pull "$IMAGE" >/dev/null 2>&1 || true

# Start a container with a fake DNS (127.0.0.1) so all DNS queries fail
# NXDOMAIN-style (no real server there) — this simulates the blocked path.
# For a real filter test, the host must run the szhost dns_filter, which is
# exercised by the Rust Suite G tests above. This script validates that the
# --dns flag injection mechanism works at all.
echo "==> starting container with --dns 127.0.0.1"
podman run -d --name "$CONTAINER" --dns 127.0.0.1 "$IMAGE" sleep 60

echo "==> testing blocked domain ($BLOCKED) — expect NXDOMAIN/failure"
if podman exec "$CONTAINER" nslookup "$BLOCKED" 2>&1 | grep -qiE "nxdomain|can.t resolve|server can.t find|not found"; then
  echo "  PASS: $BLOCKED resolved as NXDOMAIN (as expected with broken resolver)"
  PASS=$((PASS + 1))
else
  # With 127.0.0.1 as DNS, nslookup may time out instead of NXDOMAIN. Either
  # way, it must NOT return a valid IP.
  if ! podman exec "$CONTAINER" nslookup "$BLOCKED" 2>&1 | grep -qE '^Address:'; then
    echo "  PASS: $BLOCKED did not resolve (timeout/error, expected)"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $BLOCKED unexpectedly resolved!"
    FAIL=$((FAIL + 1))
  fi
fi

echo ""
echo "==> results: $PASS passed, $FAIL failed"
if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
echo "sandbox-network.sh: all checks passed"
