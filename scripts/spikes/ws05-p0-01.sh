#!/usr/bin/env bash
# WS05-P0-01 — macOS on-device cases: Apple `container` (V-AC-01..05) and the
# podman/docker engine captures (V-OCI-01..03). Run ON THE MAC, once.
# Usage: ws05-p0-01.sh OUTDIR
#   SPIKE_IMAGE   small image for the container cases (default docker.io/library/alpine:3;
#                 needs registry access the first time)
# It creates: an Apple internal network named thegn-none, containers tgspike-*, and ONE
# listener on TCP 18099 (all interfaces) for V-AC-02. All are removed by trap.
# Evidence: OUTDIR/WS05-P0-01.evidence.jsonl (one JSON line per case) + raw captures in
# OUTDIR/WS05-P0-01/<id>.out|err (these are the WS02-P3-08 fixtures for V-OCI-*).
# The `container` flag forms are the planner's suggestions, unverified: each line records
# the argv actually run. A missing tool is "skipped-not-installed", never a failed run.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"
# shellcheck source=lib-cases.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib-cases.sh"

[ "$(uname -s)" = Darwin ] || {
  echo "macOS only" >&2
  exit 2
}
sc_init WS05-P0-01 "${1:-}"
img="${SPIKE_IMAGE:-docker.io/library/alpine:3}"
net=thegn-none
port=18099

sp_on_exit "container rm -f tgspike-label >/dev/null 2>&1"
sp_on_exit "container network delete $net >/dev/null 2>&1"

# ---- V-AC-01 .. V-AC-05 (Apple container)
if command -v container >/dev/null 2>&1; then
  SC_TIMEOUT=60 sc_case V-AC-01 any container run --rm --network none "$img" true

  # V-AC-02: internal network blocks the internet AND the Mac host?
  SC_TIMEOUT=60 sc_case V-AC-02-net-create any container network create --internal "$net"
  python3 -m http.server "$port" --bind 0.0.0.0 >/dev/null 2>&1 &
  lpid=$!
  sp_on_exit "kill $lpid"
  sleep 1
  host_ip="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || echo)"
  SC_TIMEOUT=60 sc_case V-AC-02-internet rcnz container run --rm --network "$net" "$img" wget -T 5 -q -O /dev/null http://1.1.1.1/
  inet_v="$SC_LAST_VERDICT" # pass == blocked
  if [ -n "$host_ip" ]; then
    SC_TIMEOUT=60 sc_case V-AC-02-host rcnz container run --rm --network "$net" "$img" wget -T 5 -q -O /dev/null "http://$host_ip:$port/"
    host_v="$SC_LAST_VERDICT"
  else
    sc_skip V-AC-02-host "no en0/en1 address found; run by hand against the Mac's LAN address"
    host_v=skipped
  fi
  sc_case V-AC-02-net-ls any container network ls --format json
  kill "$lpid" 2>/dev/null || true

  # V-AC-03: --ulimit nproc accepted and effective?
  # shellcheck disable=SC2016 # expanded by the container's sh
  SC_TIMEOUT=90 sc_case V-AC-03 any container run --rm --ulimit nproc=64:64 "$img" sh -c 'for i in $(seq 1 200); do sleep 20 & done 2>/dev/null; sleep 1; echo procs=$(ps | grep -c "[s]leep 20")'
  procs="$(sed -n 's/^procs=//p' "$SP_OUT/$SP_ITEM/V-AC-03.out" | head -1)"

  # V-AC-04: labels accepted and visible?
  SC_TIMEOUT=60 sc_case V-AC-04-run any container run -d -l thegn.owner=x --name tgspike-label "$img" sleep 60
  sc_case V-AC-04-ls any container ls -a --format json
  if grep -q 'thegn.owner' "$SP_OUT/$SP_ITEM/V-AC-04-ls.out" 2>/dev/null; then lab="visible"; else lab="NOT visible/accepted"; fi

  sc_case V-AC-05 any container --version
else
  for c in V-AC-01 V-AC-02 V-AC-03 V-AC-04 V-AC-05; do sc_skip "$c" "Apple container CLI not installed"; done
  inet_v=skipped
  host_v=skipped
  procs=""
  lab="not run"
fi

# ---- V-OCI-01 .. V-OCI-03 (raw captures; WS02-P3-08 fixtures)
sc_case V-OCI-01-list any podman machine list --format json
sc_case V-OCI-01-inspect any podman machine inspect --format json
sc_case V-OCI-02-list any podman machine list
sc_case V-OCI-02-info any podman info
sc_case V-OCI-03-ctx any docker context ls --format json
# shellcheck disable=SC2016 # expanded by the inner sh
sc_case V-OCI-03-sockets any sh -c 'for p in "$HOME/.docker/run/docker.sock" "$HOME/.docker/desktop/docker.sock" "$HOME/.colima/default/docker.sock" "$HOME/.orbstack/run/docker.sock" "$HOME/.rd/docker.sock" "$HOME/.finch/finch.sock" "$HOME/.lima/docker/sock/docker.sock"; do ls -l "$p" 2>&1; done'

# ---- verdicts that can be computed
if [ "$inet_v" = pass ] && [ "$host_v" = pass ]; then
  ac2="positive branch for WS05-P0-02 (internet=blocked, host=blocked)"
else
  ac2="NEGATIVE branch for WS05-P0-02 (internet_blocked=$inet_v host_blocked=$host_v; pass==blocked)"
fi
sc_verdict "V-AC-02: $ac2 | V-AC-03: nproc procs=${procs:-n/a} (accepted and stops near 64 => WS05-P0-04 maps pids_limit) | V-AC-04: label $lab (visible => labels + GC filter) | V-AC-05: read the version format from V-AC-05.out for WS05-P0-05"
