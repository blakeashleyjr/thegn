#!/usr/bin/env bash
# flaky-proxy.sh — a deliberately unreliable ssh ProxyCommand for testing the
# remote-connection hardening (keepalives, transient-retry, auto-heal).
#
# Usage (in ~/.ssh/config or superzej [host.*.ssh] extra_args):
#   extra_args = ["-o", "ProxyCommand=scripts/flaky-proxy.sh %h %p"]
#
# Modes (env vars):
#   FLAKY_DROP_FIRST=N   drop the first N connection attempts outright
#                        (exercises the connect retry ladder). Attempt counter
#                        persists in $FLAKY_STATE (default /tmp/flaky-proxy.n).
#   FLAKY_KILL_AFTER=S   kill the stream after S seconds (exercises mid-
#                        transfer resume). "R" = random 3–15s per connection.
#   FLAKY_DOWN=1         refuse every connection (exercises Failed(retryable)
#                        + background heal; unset to watch the host recover).
#
# Plain `nc` underneath — swap for `tailscale nc` to test the tailnet path.
set -euo pipefail

host=$1
port=$2
state=${FLAKY_STATE:-/tmp/flaky-proxy.n}

if [[ ${FLAKY_DOWN:-0} == 1 ]]; then
  echo "flaky-proxy: DOWN (FLAKY_DOWN=1)" >&2
  exit 255
fi

if [[ -n ${FLAKY_DROP_FIRST:-} ]]; then
  n=$(cat "$state" 2>/dev/null || echo 0)
  echo $((n + 1)) >"$state"
  if ((n < FLAKY_DROP_FIRST)); then
    echo "flaky-proxy: dropping attempt $((n + 1))/$FLAKY_DROP_FIRST" >&2
    exit 255
  fi
fi

if [[ -n ${FLAKY_KILL_AFTER:-} ]]; then
  secs=$FLAKY_KILL_AFTER
  [[ $secs == R ]] && secs=$((RANDOM % 12 + 3))
  echo "flaky-proxy: stream dies in ${secs}s" >&2
  exec timeout "$secs" nc "$host" "$port"
fi

exec nc "$host" "$port"
