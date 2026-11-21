#!/usr/bin/env bash
# WS05-P0-01 — macOS on-device cases: Apple `container` (V-AC-01..05) and
# read-only podman/docker engine captures (V-OCI-01..03). Run ON THE MAC.
# Usage: ws05-p0-01.sh OUTDIR
# SPIKE_IMAGE may select a small image; the default is docker.io/library/alpine:3.
# The script serves only a random canary body from a random LAN port and removes
# only resources named with its private per-run nonce. It never serves a cwd.
set -euo pipefail
umask 077
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../test/macos/lib.sh
# shellcheck disable=SC1091 # shellcheck runs without -x
. "$here/../../test/macos/lib.sh"

[ "$(uname -s)" = Darwin ] || {
  echo "macOS only" >&2
  exit 2
}
sc_init WS05-P0-01 "${1:-}"
SC_SUMMARY_PRIVATE=1
export SC_SUMMARY_PRIVATE

img="${SPIKE_IMAGE:-docker.io/library/alpine:3}"
run_dir="$(mktemp -d "${TMPDIR:-/tmp}/thegn-ws05.XXXXXX")"
sp_on_exit "rm -rf '$run_dir'"
run_id="$(basename "$run_dir" | tail -c 10 | macos_ws05_resource_id)"
[ -n "$run_id" ] || sp_die "could not generate a per-run resource id"
net="thegn-ws05-$run_id"
label_container="thegn-ws05-$run_id-label"
export SP_REDACT_EXTRA_1="$img"

# A fixed, inert response at an ephemeral port on one concrete host interface.
# No document root, directory listing, or user files are exposed.
host_iface="$(route -n get default 2>/dev/null | awk '/interface:/{print $2; exit}')"
host_ip=""
if [ -n "$host_iface" ]; then
  host_ip="$(ipconfig getifaddr "$host_iface" 2>/dev/null || true)"
fi
if [ -z "$host_ip" ]; then
  for host_iface in en0 en1; do
    host_ip="$(ipconfig getifaddr "$host_iface" 2>/dev/null || true)"
    [ -n "$host_ip" ] && break
  done
fi
export SP_REDACT_EXTRA_2="$host_ip"

# shellcheck disable=SC2016 # variables expand in the container's sh, not here
probe_script='tmp=$(mktemp); err=$(mktemp); wget -T 5 -O "$tmp" "$1" 2>"$err"; rc=$?; if [ "$rc" -eq 0 ] && [ -n "$2" ]; then if [ "$(cat "$tmp")" = "$2" ]; then body=expected; else body=unexpected; fi; else body=none; fi; if grep -Eiq "network is unreachable|no route to host|operation not permitted|network is down" "$err"; then kind=denied; elif [ "$rc" -eq 0 ]; then kind=connected; elif grep -Eiq "server returned error|HTTP/[0-9.]+ [45][0-9][0-9]" "$err"; then kind=reachable; elif grep -Eiq "timed out|timeout" "$err"; then kind=timeout; else kind=failed; fi; printf "__THEGN_SPIKE_RC=%s\n__THEGN_SPIKE_KIND=%s\n__THEGN_SPIKE_BODY=%s\n" "$rc" "$kind" "$body"; rm -f "$tmp" "$err"; exit 0'

probe_row() { # ID [network-name|default] URL [expected body]
  local id="$1" network="$2" url="$3" body="${4:-}"
  if [ "$network" = default ]; then
    SC_TIMEOUT=45 sc_case "$id" any container run --rm "$img" sh -c "$probe_script" sh "$url" "$body"
  else
    SC_TIMEOUT=45 sc_case "$id" any container run --rm --network "$network" "$img" sh -c "$probe_script" sh "$url" "$body"
  fi
}

probe_value() { # ID FIELD -> the last recorded marker for that case
  sed -n "s/^__THEGN_SPIKE_$2=//p" "$SP_OUT/$SP_ITEM/$1.out" | tail -1
}

# ---- V-AC-01 .. V-AC-05 (Apple container)
if command -v container >/dev/null 2>&1; then
  SC_TIMEOUT=60 sc_case V-AC-01-startup-control any container run --rm "$img" true
  startup_rc="$SC_LAST_RC"
  SC_TIMEOUT=60 sc_case V-AC-01 any container run --rm --network none "$img" true
  none_rc="$SC_LAST_RC"
  if [ "$startup_rc" -eq 0 ] && [ "$none_rc" -eq 0 ]; then
    network_none_verdict="recorded: runtime and --network none run succeeded"
  elif [ "$startup_rc" -eq 0 ]; then
    network_none_verdict="not accepted or failed: default startup control passed but --network none failed"
  else
    network_none_verdict="inconclusive: default startup control failed"
  fi

  SC_TIMEOUT=60 sc_case V-AC-02-net-create any container network create --internal "$net"
  net_rc="$SC_LAST_RC"
  if [ "$net_rc" -eq 0 ]; then
    sp_on_exit "container network delete '$net'"
  fi

  if [ -n "$host_ip" ]; then
    token="thegn-canary-$run_id"
    port_file="$run_dir/port"
    listener="$run_dir/listener.py"
    cat >"$listener" <<'PY'
import http.server, sys
address, marker, port_file = sys.argv[1:]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/" + marker:
            self.send_error(404)
            return
        body = marker.encode("ascii")
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *_args):
        pass

server = http.server.HTTPServer((address, 0), Handler)
with open(port_file, "w") as f:
    f.write(str(server.server_port))
server.serve_forever()
PY
    python3 "$listener" "$host_ip" "$token" "$port_file" >"$run_dir/listener.out" 2>"$run_dir/listener.err" &
    listener_pid=$!
    sp_on_exit "kill '$listener_pid'"
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      [ -s "$port_file" ] && break
      sleep 0.2
    done
    if [ -s "$port_file" ]; then
      port="$(cat "$port_file")"
      host_url="http://$host_ip:$port/$token"
      probe_row V-AC-02-host-control default "$host_url" "$token"
      host_control_rc="$SC_LAST_RC"
      host_control_kind="$(probe_value V-AC-02-host-control KIND)"
      host_control_body="$(probe_value V-AC-02-host-control BODY)"
    else
      sc_line V-AC-02-host-control "listener did not bind" -1 positive-control "" "" "private listener failed to start" inconclusive
      host_control_rc=255
      host_control_kind=missing
      host_control_body=none
    fi
  else
    sc_line V-AC-02-host-control "no host interface address" -1 positive-control "" "" "could not choose a specific host bind address" inconclusive
    host_control_rc=255
    host_control_kind=missing
    host_control_body=none
  fi

  # A successful ordinary-network request proves the image/runtime can start and
  # reach the internet before an internal-network failure is called a block.
  probe_row V-AC-02-internet-control default http://1.1.1.1/
  internet_control_rc="$SC_LAST_RC"
  internet_control_kind="$(probe_value V-AC-02-internet-control KIND)"

  if [ "$net_rc" -eq 0 ]; then
    probe_row V-AC-02-internet "$net" http://1.1.1.1/
    internal_internet_rc="$SC_LAST_RC"
    internal_internet_kind="$(probe_value V-AC-02-internet KIND)"
    if [ -n "$host_ip" ] && [ -n "${host_url:-}" ]; then
      probe_row V-AC-02-host "$net" "$host_url" "$token"
      internal_host_rc="$SC_LAST_RC"
      internal_host_kind="$(probe_value V-AC-02-host KIND)"
      internal_host_body="$(probe_value V-AC-02-host BODY)"
    else
      sc_line V-AC-02-host "no host control URL" -1 positive-control "" "" "host listener unavailable" inconclusive
      internal_host_rc=255
      internal_host_kind=missing
      internal_host_body=none
    fi
  else
    sc_line V-AC-02-internet "internal network was not created" -1 positive-control "" "" "cannot test egress without the network" inconclusive
    sc_line V-AC-02-host "internal network was not created" -1 positive-control "" "" "cannot test host reach without the network" inconclusive
    internal_internet_rc=255
    internal_internet_kind=missing
    internal_host_rc=255
    internal_host_kind=missing
    internal_host_body=none
  fi

  ac2="$(macos_ws05_network_verdict "$net_rc" "$host_control_rc" "$host_control_kind" "$host_control_body" \
    "$internet_control_rc" "$internet_control_kind" "$internal_internet_rc" "$internal_internet_kind" \
    "$internal_host_rc" "$internal_host_kind" "$internal_host_body")"

  # V-AC-03: successful run plus a printed process count is needed before
  # claiming the guest enforces nproc; a setup error is only recorded.
  # shellcheck disable=SC2016 # variables expand in the container's sh, not here
  SC_TIMEOUT=90 sc_case V-AC-03 any container run --rm --ulimit nproc=64:64 "$img" sh -c 'for i in $(seq 1 200); do sleep 20 & done 2>/dev/null; sleep 1; echo procs=$(ps | grep -c "[s]leep 20")'
  ulimit_rc="$SC_LAST_RC"
  procs="$(sed -n 's/^procs=//p' "$SP_OUT/$SP_ITEM/V-AC-03.out" | head -1)"
  if [ "$ulimit_rc" -eq 0 ] && [ -n "$procs" ]; then
    ulimit_verdict="recorded: accepted; observed process count $procs under the image default user; policy effect unverified"
  else
    ulimit_verdict="inconclusive: container start or process probe failed"
  fi

  # V-AC-04 uses only this run's container. The wrapper filters parsed JSON
  # before writing anything, so unrelated container names never enter evidence.
  SC_TIMEOUT=60 sc_case V-AC-04-run any container run -d -l "thegn.spike=$run_id" --name "$label_container" "$img" sleep 60
  label_run_rc="$SC_LAST_RC"
  if [ "$label_run_rc" -eq 0 ]; then
    sp_on_exit "container rm -f '$label_container'"
  fi
  sc_case V-AC-04-ls any python3 "$here/../../test/macos/apple-container-list-probe.py" "$label_container" "$run_id"
  label_list_rc="$(probe_value V-AC-04-ls LIST_RC)"
  label_list_parseable="$(probe_value V-AC-04-ls LIST_PARSEABLE)"
  label_list_name="$(probe_value V-AC-04-ls LIST_NAME)"
  label_list_visible="$(probe_value V-AC-04-ls LIST_LABEL)"
  if [ "$label_run_rc" -eq 0 ] && [ "$label_list_rc" = 0 ] &&
    [ "$label_list_parseable" = true ] && [ "$label_list_name" = true ] &&
    [ "$label_list_visible" = true ]; then
    label_verdict="recorded: this run's container was listed with its label"
  elif [ "$label_run_rc" -eq 0 ] && [ "$label_list_rc" = 0 ] &&
    [ "$label_list_parseable" = true ] && [ "$label_list_name" = true ]; then
    label_verdict="not visible: this run's listed container had no matching label"
  else
    label_verdict="inconclusive: container start, JSON parsing, or exact-name listing did not prove label visibility"
  fi

  sc_case V-AC-05 any container --version
else
  for c in V-AC-01-startup-control V-AC-01 V-AC-02-net-create V-AC-02-host-control V-AC-02-internet-control V-AC-02-internet V-AC-02-host V-AC-03 V-AC-04-run V-AC-04-ls V-AC-05; do
    sc_skip "$c" "Apple container CLI not installed"
  done
  network_none_verdict="inconclusive: Apple container CLI not installed"
  ac2="inconclusive: Apple container CLI not installed"
  ulimit_verdict="inconclusive: Apple container CLI not installed"
  label_verdict="inconclusive: Apple container CLI not installed"
fi

# ---- V-OCI-01 .. V-OCI-03 (read-only captures; WS02-P3-08 fixtures)
sc_case V-OCI-01-list any podman machine list --format json
sc_case V-OCI-01-inspect any podman machine inspect --format json
sc_case V-OCI-02-list any podman machine list
sc_case V-OCI-02-info any podman info
sc_case V-OCI-03-ctx any docker context ls --format json
# shellcheck disable=SC2016 # $HOME expands in the child shell
sc_case V-OCI-03-sockets any sh -c 'for p in "$HOME/.docker/run/docker.sock" "$HOME/.docker/desktop/docker.sock" "$HOME/.colima/default/docker.sock" "$HOME/.orbstack/run/docker.sock" "$HOME/.rd/docker.sock" "$HOME/.finch/finch.sock" "$HOME/.lima/docker/sock/docker.sock"; do ls -l "$p" 2>&1; done'

sc_verdict "V-AC-01: $network_none_verdict | V-AC-02: $ac2 | V-AC-03: $ulimit_verdict | V-AC-04: $label_verdict | V-AC-05: read version format from V-AC-05.out | V-OCI-01..03: review redacted captures; missing CLIs are skipped"
