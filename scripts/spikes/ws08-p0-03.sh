#!/usr/bin/env bash
# WS08-P0-03 — check the twelve hard-coded machine0 MCP tool names against the live
# server's tools/list. NEEDS A MACHINE0 ACCOUNT. Read-only: lists tools, creates no VM.
# Usage: MACHINE0_API_KEY=... [MACHINE0_MCP_URL=https://app.machine0.io/mcp] ws08-p0-03.sh OUTDIR
# The key goes to curl through a 0600 config file (never argv, never saved). Output:
# OUTDIR/WS08-P0-03/machine0_tools_list.json (the tool names + the raw tools/list pages,
# no secrets; this is the fixture for crates/thegn-svc/tests/fixtures/machine0_tools_list.json
# once reviewed) and the drift verdict. The implementation (Mcp0Client::list_tools,
# tool_drift) is the real item; this proves the live answer first.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS08-P0-03
sp_init "$item" "${1:-}"
sp_need curl jq
sp_need_env MACHINE0_API_KEY
url="${MACHINE0_MCP_URL:-https://app.machine0.io/mcp}"
sp_kv endpoint "$url"

# the names thegn calls today (crates/thegn-svc/src/machine0/mod.rs, `mod tool`)
expected="vm_create vm_get_by_name vm_list vm_destroy vm_start vm_stop vm_suspend ssh_key_create_public ssh_key_list image_create image_list size_list"
sp_kv expected_tools "$expected"

sec="$SP_OUT/$item/.secrets"
mkdir -p "$sec"
chmod 700 "$sec"
sp_on_exit "rm -rf '$sec'"
(
  umask 077
  printf 'header = "x-api-key: %s"\nheader = "Content-Type: application/json"\nheader = "Accept: application/json, text/event-stream"\n' "$MACHINE0_API_KEY" >"$sec/auth.curl"
  : >"$sec/session.curl"
)

# rpc BODY OUTFILE -> prints HTTP status; writes the JSON payload (SSE unwrapped) to OUTFILE
rpc() {
  local code ct
  code="$(curl -sS -m 60 -K "$sec/auth.curl" -K "$sec/session.curl" -D "$sec/h.txt" -o "$sec/raw.txt" -w '%{http_code}' -X POST "$url" -d "$1")"
  ct="$(tr -d '\r' <"$sec/h.txt" | sed -n 's/^[Cc]ontent-[Tt]ype: *//p' | head -n1)"
  case "$ct" in
  text/event-stream*) sed -n 's/^data: *//p' "$sec/raw.txt" | tail -n1 >"$2" ;;
  *) cp "$sec/raw.txt" "$2" ;;
  esac
  printf '%s' "$code"
}

c="$(rpc '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"thegn-spike","version":"0"}}}' "$sec/init.json")"
sp_kv initialize_status "$c"
sid="$(tr -d '\r' <"$sec/h.txt" | sed -n 's/^[Mm]cp-[Ss]ession-[Ii]d: *//p' | head -n1)"
if [ -n "$sid" ]; then
  (
    umask 077
    printf 'header = "Mcp-Session-Id: %s"\n' "$sid" >"$sec/session.curl"
  )
  sp_kv mcp_session_id "present (value not recorded)"
else
  sp_kv mcp_session_id "absent"
fi
[ "$c" = 200 ] || [ "$c" = 202 ] || sp_die "initialize failed with HTTP $c (bad key? wrong URL?)"
sp_kv server_info "$(jq -c '.result.serverInfo // .result // .error' "$sec/init.json" 2>/dev/null | head -c 300)"
rpc '{"jsonrpc":"2.0","method":"notifications/initialized"}' "$sec/ni.json" >/dev/null || true

: >"$sec/names.txt"
pages=0
cursor=""
while [ "$pages" -lt 10 ]; do
  pages=$((pages + 1))
  if [ -z "$cursor" ]; then
    body='{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  else
    body="$(jq -nc --arg c "$cursor" '{jsonrpc:"2.0",id:2,method:"tools/list",params:{cursor:$c}}')"
  fi
  c="$(rpc "$body" "$sec/page.json")"
  [ "$c" = 200 ] || sp_die "tools/list failed with HTTP $c"
  jq -r '.result.tools[]?.name' "$sec/page.json" >>"$sec/names.txt"
  cp "$sec/page.json" "$SP_OUT/$item/page-$pages.json"
  cursor="$(jq -r '.result.nextCursor // empty' "$sec/page.json")"
  [ -n "$cursor" ] || break
done
sort -u "$sec/names.txt" >"$SP_OUT/$item/listed.txt"
sp_kv pages "$pages"
sp_kv listed_count "$(wc -l <"$SP_OUT/$item/listed.txt" | tr -d ' ')"
sp_note "listed tools:"
cat "$SP_OUT/$item/listed.txt" >>"$SP_EVID"

missing=""
for t in $expected; do
  grep -qx "$t" "$SP_OUT/$item/listed.txt" || missing="$missing $t"
done
jq -R . "$SP_OUT/$item/listed.txt" | jq -s '{tools: .}' >"$SP_OUT/$item/machine0_tools_list.json"
sp_kv fixture "$SP_OUT/$item/machine0_tools_list.json (names only; raw pages in page-N.json: review, no secrets expected)"

if [ -z "$missing" ]; then
  v="drift EMPTY: all 12 names present (record the capture date; fix the two contradictory doc comments in machine0/mod.rs)"
else
  v="drift NON-EMPTY, missing:$missing (fix the constants one line each, re-run until empty)"
fi
sp_verdict "tool_drift(listed) must be empty; a rename shows as a missing name; extra tools are not an error" "$v"
