#!/usr/bin/env bash
# Shared macOS on-device evidence helpers. Bash 3.2 compatible.
set -euo pipefail

MACOS_TEST_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# The spike scripts and the permanent runner share one evidence contract.
# shellcheck source=scripts/spikes/lib.sh
# shellcheck disable=SC1091 # source path is resolved from the checkout root
. "$MACOS_TEST_ROOT/scripts/spikes/lib.sh"
# shellcheck source=scripts/spikes/lib-cases.sh
# shellcheck disable=SC1091 # source path is resolved from the checkout root
. "$MACOS_TEST_ROOT/scripts/spikes/lib-cases.sh"

# The older spike helpers also serve unrelated procedures and stay Python-free.
# Override redaction only in this Mac case library, where Python is already a
# declared prerequisite and host addresses/image refs must be hidden.
sp_redact() {
  python3 -c '
import os, re, sys
text = sys.stdin.read()
home = os.environ.get("HOME", "")
user = os.environ.get("SP_REDACT_USER") or os.environ.get("USER", "")
host = os.environ.get("SP_REDACT_HOST", "")
values = [
    (home, "<HOME>"),
    (f"/Users/{user}", "/Users/<USER>"),
    (f"/home/{user}", "/home/<USER>"),
    (os.environ.get("SP_REDACT_EXTRA_1", ""), "<LOCAL>"),
    (os.environ.get("SP_REDACT_EXTRA_2", ""), "<LOCAL>"),
]
for value, replacement in sorted(values, key=lambda pair: len(pair[0]), reverse=True):
    if value:
        text = text.replace(value, replacement)
for value, replacement in ((user, "<USER>"), (host, "<HOST>")):
    if value:
        text = re.sub(r"(?<![A-Za-z0-9_.-])" + re.escape(value) + r"(?![A-Za-z0-9_.-])", replacement, text)
sys.stdout.write(text)
'
}

macos_ws05_network_verdict() {
  local network_rc="$1" host_rc="$2" host_kind="$3" host_body="$4"
  local internet_control_rc="$5" internet_control_kind="$6"
  local internet_rc="$7" internet_kind="$8" internal_host_rc="$9"
  local internal_host_kind="${10}" internal_host_body="${11}"

  if [ "$network_rc" -eq 0 ] && [ "$host_rc" -eq 0 ] &&
    [ "$host_kind" = connected ] && [ "$host_body" = expected ] &&
    [ "$internet_control_rc" -eq 0 ] &&
    { [ "$internet_control_kind" = connected ] || [ "$internet_control_kind" = reachable ]; } &&
    [ "$internet_rc" -eq 0 ] && [ "$internet_kind" = denied ] &&
    [ "$internal_host_rc" -eq 0 ] && [ "$internal_host_kind" = denied ]; then
    printf '%s\n' "verified: ordinary network reached both controls; internal network returned explicit network-denied errors for internet and host"
  elif [ "$internet_kind" = connected ] || [ "$internet_kind" = reachable ] || [ "$internal_host_body" = expected ]; then
    printf '%s\n' "not isolated: internal network reached an external or host control"
  else
    printf '%s\n' "inconclusive: network setup, positive controls, or internal denial could not be verified; inspect per-case rows"
  fi
}
