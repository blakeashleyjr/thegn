#!/usr/bin/env bash
# 00-bwrap-userns-probe.sh — can this host run bubblewrap in a user namespace?
#
# Passes when `bwrap --unshare-user --unshare-pid` can run /bin/true. Skips (77)
# when bwrap is missing or user namespaces are unavailable (for example an
# AppArmor profile restricting unprivileged user namespaces). Later suites in
# this directory may assume the probe passed; with THEGN_REQUIRE_SECURITY_TESTS=1
# the driver turns the skip into a failure.
set -euo pipefail

if ! command -v bwrap >/dev/null 2>&1; then
  echo "bwrap not found on PATH" >&2
  exit 77
fi

if bwrap --unshare-user --unshare-pid --ro-bind / / -- /bin/true 2>/dev/null; then
  exit 0
fi

echo "bwrap cannot create a user namespace here (userns disabled or restricted)" >&2
exit 77
