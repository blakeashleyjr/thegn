#!/usr/bin/env bash
# test/userns-e2e.sh — driver for the user-namespace / confinement suites.
#
# Runs every executable test/userns-e2e.d/*.sh in lexical order. A suite exits
#   0   pass
#   77  skip (it prints the reason on stderr)
#   any other status: fail
# With THEGN_REQUIRE_SECURITY_TESTS=1 a skip fails the run, so a CI job that
# must prove confinement cannot go green by skipping. The suite registry is
# shared by the sandbox + remote programme: owners append a suite by adding a
# file here (see CONTRIBUTING.md, "Programme conventions"). Dispatch/nightly
# only (the `security-e2e` job); never a pull-request gate.
#
# Usage: test/userns-e2e.sh      (or: just sandbox-userns-e2e)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dir="$here/userns-e2e.d"
require="${THEGN_REQUIRE_SECURITY_TESTS:-0}"

pass=0
skip=0
fail=0
failed=()

shopt -s nullglob
for suite in "$dir"/*.sh; do
  [[ -x $suite ]] || continue
  name="$(basename "$suite")"
  echo "== $name"
  status=0
  "$suite" || status=$?
  case "$status" in
  0)
    pass=$((pass + 1))
    ;;
  77)
    skip=$((skip + 1))
    echo "SKIP $name" >&2
    if [[ $require == 1 ]]; then
      fail=$((fail + 1))
      failed+=("$name (skipped, but THEGN_REQUIRE_SECURITY_TESTS=1)")
    fi
    ;;
  *)
    fail=$((fail + 1))
    failed+=("$name (exit $status)")
    ;;
  esac
done

echo "userns-e2e: $pass passed, $skip skipped, $fail failed"
if [[ $fail -gt 0 ]]; then
  printf 'FAILED: %s\n' "${failed[@]}" >&2
  exit 1
fi
