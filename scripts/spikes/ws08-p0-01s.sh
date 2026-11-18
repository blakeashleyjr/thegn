#!/usr/bin/env bash
# WS08-P0-01s — Daytona ssh token gateway: how tokens are minted and whether
# commands/PTYs run. NEEDS A DAYTONA ACCOUNT (free or paid). Creates ONE throwaway
# sandbox (may cost a few cents) and deletes it by trap; revokes tokens on exit.
# Usage: DAYTONA_API_KEY=... [DAYTONA_API_BASE=https://app.daytona.io/api] \
#        [DAYTONA_SNAPSHOT=<snapshot>] ws08-p0-01s.sh OUTDIR
# The API key and the minted token NEVER appear on argv: curl reads headers and
# token-bearing URLs from a 0600 config file, ssh reads the token from a 0600
# ssh_config `User` line. Everything saved is redacted. The REST path/body/field names
# are the plan's CANDIDATES (POST /sandbox/{id}/ssh-access and POST /ssh-access); each
# try's status code and shape is recorded either way.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS08-P0-01s
sp_init "$item" "${1:-}"
sp_need curl jq ssh ssh-keyscan
sp_need_env DAYTONA_API_KEY
base="${DAYTONA_API_BASE:-https://app.daytona.io/api}"
gw=ssh.app.daytona.io

sec="$SP_OUT/$item/.secrets"
mkdir -p "$sec"
chmod 700 "$sec"
sp_on_exit "rm -rf '$sec'"
auth="$sec/auth.curl"
(
  umask 077
  printf 'header = "Authorization: Bearer %s"\nheader = "Content-Type: application/json"\n' "$DAYTONA_API_KEY" >"$auth"
)

sp_kv api_base "$base"
sp_kv ssh_version "$(ssh -V 2>&1)"
sp_kv daytona_cli "$(daytona --version 2>&1 | head -n1 || echo absent)"
sp_kv daytona_v0_cli "$(daytona-v0 --version 2>&1 | head -n1 || echo absent)"

# api METHOD PATH [JSON_BODY] -> body in $sec/resp.json, status code echoed
api() {
  local m="$1" p="$2" body="${3:-}" cfg="$sec/req.curl" code
  (
    umask 077
    printf 'url = "%s%s"\nrequest = "%s"\n' "$base" "$p" "$m" >"$cfg"
    [ -z "$body" ] || printf 'data = %s\n' "$(printf '%s' "$body" | jq -Rs .)" >>"$cfg"
  )
  code="$(curl -sS -m 60 -K "$auth" -K "$cfg" -o "$sec/resp.json" -w '%{http_code}' 2>"$sec/curl.err" || echo 000)"
  printf '%s' "$code"
}

# Redact any token-ish value in a JSON file; tolerate non-JSON bodies.
redact_json() {
  jq 'walk(if type=="object" then with_entries(if (.key|test("token|secret|password|authorization";"i")) then .value="<TOKEN>" else . end) else . end)' "$1" 2>/dev/null |
    sed -E 's/[A-Za-z0-9_.-]{24,}@ssh\./<TOKEN>@ssh./g' || sed -E 's/[A-Za-z0-9_.-]{24,}/<REDACTED>/g' "$1"
}

# 1. create a throwaway sandbox
create_body='{}'
[ -z "${DAYTONA_SNAPSHOT:-}" ] || create_body="$(jq -nc --arg s "$DAYTONA_SNAPSHOT" '{snapshot:$s}')"
code="$(api POST /sandbox "$create_body")"
sp_kv create_status "$code"
id="$(jq -r '.id // .sandboxId // empty' "$sec/resp.json" 2>/dev/null || true)"
[ -n "$id" ] || {
  redact_json "$sec/resp.json" >>"$SP_EVID" || true
  sp_die "sandbox create failed (status $code); nothing to clean up"
}
sp_on_exit "api DELETE /sandbox/$id >/dev/null"
sp_kv sandbox_id_prefix "${id:0:6}..."

# 2/3. mint attempts: record path, body, status, shape, token field, expiry
token=""
minted_via=""
for try in "POST|/sandbox/$id/ssh-access|" "POST|/sandbox/$id/ssh-access|{\"expiresInMinutes\":60}" "POST|/ssh-access|{\"sandboxId\":\"$id\"}" "POST|/ssh-access?sandboxId=$id|"; do
  m="${try%%|*}"
  rest="${try#*|}"
  p="${rest%%|*}"
  b="${rest#*|}"
  code="$(api "$m" "$p" "$b")"
  sp_note ""
  sp_note "## mint try: $m ${p//$id/<ID>} body=${b//$id/<ID>} -> status $code"
  redact_json "$sec/resp.json" | head -c 1500 >>"$SP_EVID" || true
  sp_note ""
  if [ "$code" -ge 200 ] && [ "$code" -lt 300 ]; then
    t="$(jq -r '(.token // .sshToken // .accessToken // (.sshCommand // "" | capture("(?<t>[^@ ]+)@ssh").t) // empty)' "$sec/resp.json" 2>/dev/null || true)"
    if [ -n "$t" ]; then
      token="$t"
      minted_via="$m $p"
      redact_json "$sec/resp.json" >"$SP_OUT/$item/daytona_ssh_access.json"
      sp_kv expiry_fields "$(jq -c '{expiresAt:.expiresAt, expiresInMinutes:.expiresInMinutes, expires:.expires, expiry:.expiry}' "$sec/resp.json" 2>/dev/null)"
      break
    fi
  fi
done
sp_kv mint_path "${minted_via:-NONE}"
sp_kv fixture "$SP_OUT/$item/daytona_ssh_access.json (redacted; copy to crates/thegn-svc/tests/fixtures/ after review)"

# 7. host key stability (independent of the token)
ssh-keyscan -T 10 -t ed25519,rsa "$gw" 2>/dev/null | ssh-keygen -lf - >"$SP_OUT/$item/hostkey1.txt" 2>&1 || true
ssh-keyscan -T 10 -t ed25519,rsa "$gw" 2>/dev/null | ssh-keygen -lf - >"$SP_OUT/$item/hostkey2.txt" 2>&1 || true
sp_note "## host key fingerprints (compare with the Daytona docs by hand)"
cat "$SP_OUT/$item/hostkey1.txt" >>"$SP_EVID"
if cmp -s "$SP_OUT/$item/hostkey1.txt" "$SP_OUT/$item/hostkey2.txt"; then sp_kv hostkey_stable_across_two_scans yes; else sp_kv hostkey_stable_across_two_scans NO; fi

if [ -z "$token" ]; then
  sp_verdict "no API mint => WS08-P0-01a reduces to the daytona-v0 path and drops the gateway" "no API mint (no 2xx response with an extractable token on any candidate path)"
  exit 0
fi

# 8/5/6. token only in a 0600 ssh_config User line; never in argv
cfg="$sec/ssh_config"
(
  umask 077
  printf 'Host tg-daytona-spike\n  HostName %s\n  User %s\n  StrictHostKeyChecking accept-new\n  UserKnownHostsFile %s/known_hosts\n  BatchMode yes\n  ConnectTimeout 15\n' "$gw" "$token" "$sec" >"$cfg"
)
sp_cap c5-nonpty 60 ssh -F "$cfg" tg-daytona-spike -- echo ok
nonpty_rc=$SP_RC
nonpty_ok=no
grep -qx ok "$SP_OUT/$item/c5-nonpty.out" && nonpty_ok=yes
sp_cap c6-pty 60 ssh -tt -F "$cfg" tg-daytona-spike -- tty
pty_rc=$SP_RC
pty_ok=no
grep -q '/dev/pts\|/dev/tty' "$SP_OUT/$item/c6-pty.out" && pty_ok=yes

# argv check while a command runs (check 8)
ssh -F "$cfg" tg-daytona-spike -- sleep 6 >/dev/null 2>&1 &
spid=$!
sleep 2
# shellcheck disable=SC2009 # the ps listing IS the evidence
if ps -eo args | grep -F "$token" | grep -v grep >/dev/null 2>&1; then argv=LEAK; else argv="no token in argv"; fi
kill "$spid" 2>/dev/null || true
wait "$spid" 2>/dev/null || true
sp_kv argv_check "$argv"

# 4. revocation: candidates; then the token must be rejected
revoked=no
for try in "DELETE|/sandbox/$id/ssh-access?token=$token" "DELETE|/ssh-access?token=$token"; do
  m="${try%%|*}"
  p="${try#*|}"
  code="$(api "$m" "$p")"
  sp_note "revoke try: $m ${p//$token/<TOKEN>} -> status $code"
  [ "$code" -ge 200 ] && [ "$code" -lt 300 ] && {
    revoked="$m ${p%%\?*}"
    break
  }
done
sp_kv revoke_call "$revoked"
if [ "$revoked" != no ]; then
  sp_cap c4-after-revoke 40 ssh -F "$cfg" tg-daytona-spike -- echo should-fail
  sp_kv token_rejected_after_revoke "$([ "$SP_RC" -ne 0 ] && echo yes || echo NO)"
fi

if [ "$argv" = LEAK ]; then
  res="STOP: token visible in argv - the file-only route failed (check 8); report before WS08-P0-01a starts"
elif [ "$nonpty_ok" = yes ] && [ "$nonpty_rc" -eq 0 ]; then
  res="gateway accepts commands (non-pty echo ok; pty=$pty_ok)"
elif [ "$pty_ok" = yes ]; then
  res="PTY only (non-pty rc=$nonpty_rc; pty works)"
else
  res="inconclusive: non-pty rc=$nonpty_rc pty rc=$pty_rc; read the captures"
fi
sp_verdict "commands accepted => WS08-P0-01a as planned; PTY only => daytona-v0 exec for non-PTY control; no API mint => daytona-v0 only; token must never be in argv" "$res"
