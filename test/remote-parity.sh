#!/usr/bin/env bash
# Provider-neutral interactive remote parity run. Plan mode is offline.
# Usage: THEGN_CHANNEL=dev test/remote-parity.sh --env <name> --i-accept-spend
#        [--keep-going] [--samples N] [--idle-probe] [--self-test]
set -euo pipefail

THEGN_BIN="${THEGN_BIN:-thegn}"
env_name=""
accept=0
keep_going=0
samples=20
idle_probe=0
self_test=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --env)
      [ "$#" -ge 2 ] || { echo "--env needs a value" >&2; exit 2; }
      env_name="$2"
      shift 2
      ;;
    --i-accept-spend) accept=1; shift ;;
    --keep-going) keep_going=1; shift ;;
    --samples)
      [ "$#" -ge 2 ] || { echo "--samples needs a value" >&2; exit 2; }
      samples="$2"
      case "$samples" in
        ''|*[!0-9]*) echo "--samples needs a whole number" >&2; exit 2 ;;
      esac
      [ "$samples" -gt 0 ] || { echo "--samples needs a positive number" >&2; exit 2; }
      shift 2
      ;;
    --idle-probe) idle_probe=1; shift ;;
    --self-test) self_test=1; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

print_plan() {
  cat <<PLAN
remote-parity plan (nothing is created without both --env <name> and
--i-accept-spend, plus THEGN_CHANNEL=dev)

Will create : ONE provider sandbox via [env.$env_name] in fresh XDG_STATE_HOME
              and THEGN_DIR, with a scratch git repo on branch rp/parity.
              No agent is started and no model usage is spent.
Max spend   : one sprite for about 10 minutes; --idle-probe adds 2.5 minutes.
Teardown    : close both sessions, stop the daemon, env down, deprovision only
              this run's ledger records, env reap --provider <kind> --apply --yes,
              then verify fleet list --env <name> has no owned sandbox.
Steps:
  P00 preflight and daemon-session token visibility
  P01 prompt startup latency (120 s timeout)
  P02 identity, terminfo and locale       P03 size and resize
  P04 colours and ANSI snapshot           P05 UTF-8, wide and cursor column
  P06 Ctrl-C                              P07 job control
  P08 Ctrl-\\                              P09 EOF
  P10 bracketed paste                     P11 full-screen program
  P12 arrow keys / DECCKM                 P13 SGR mouse
  P14 alternate-screen restore            P15 output flood
  P16 1 MiB paste                         P17 title
  P18 cwd and OSC 7                       P19 exit status
  P20 echo latency ($samples samples)     P21 startup step table
  P22 remote close leak check             P23 idle probe (only --idle-probe)
Output     : one PASS|WARN|FAIL line per case, a summary table, and FAILED CASES.
Known quirk: THE-2450 is reported as WARN when only the first tty line ending differs.
PLAN
}

self_test_run() {
  local test_tmp daemon_pid="" local_session="" remote_session="" status
  test_tmp="$(mktemp -d)"
  export XDG_STATE_HOME="$test_tmp/state" THEGN_DIR="$test_tmp/thegn"
  unset THEGN_CONFIG_DIR THEGN_CONTROL_ADDR THEGN_CONTROL_TOKEN
  mkdir -p "$XDG_STATE_HOME" "$THEGN_DIR" "$test_tmp/repo"

  self_test_cleanup() {
    local rc="${1:-$?}"
    trap - EXIT INT TERM
    set +e
    [ -z "$local_session" ] || "$THEGN_BIN" session close "$local_session" --json >/dev/null 2>&1 || true
    [ -z "$remote_session" ] || "$THEGN_BIN" session close "$remote_session" --json >/dev/null 2>&1 || true
    "$THEGN_BIN" daemon stop >/dev/null 2>&1 || true
    if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
      kill "$daemon_pid" 2>/dev/null || true
      wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$test_tmp"
    exit "$rc"
  }
  trap self_test_cleanup EXIT INT TERM

  echo "remote-parity self-test: two local sessions, no provider or network"
  "$THEGN_BIN" daemon >"$test_tmp/daemon.log" 2>&1 &
  daemon_pid=$!
  daemon_ready=0
  for _ in $(seq 1 30); do
    if "$THEGN_BIN" session list --json >/dev/null 2>&1; then
      daemon_ready=1
      break
    fi
    sleep 0.5
  done
  if [ "$daemon_ready" -ne 1 ]; then
    echo "self-test: daemon did not answer session list within 15 s" >&2
    return 1
  fi

  self_argv_json="$(python3 -I -c 'import json; print(json.dumps(["bash", "--norc", "--noprofile", "-i"]))')"
  self_open_params="$(SELF_ARGV_JSON="$self_argv_json" SELF_OPEN_CWD="$test_tmp/repo" python3 -I -c '
import json, os
print(json.dumps({"argv":json.loads(os.environ["SELF_ARGV_JSON"]),"cwd":os.environ["SELF_OPEN_CWD"],"env":[["PS1","PARITY$ "]],"rows":40,"cols":120}, separators=(",", ":")))
')"
  local_open_json="$("$THEGN_BIN" api call sessions.open --params "$self_open_params")"
  local_session="$(printf '%s' "$local_open_json" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
  remote_open_json="$("$THEGN_BIN" api call sessions.open --params "$self_open_params")"
  remote_session="$(printf '%s' "$remote_open_json" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["id"])')"

  export RP_THEGN_BIN="$THEGN_BIN" RP_LOCAL_SESSION="$local_session" RP_REMOTE_SESSION="$remote_session" RP_REPO="$test_tmp/repo" RP_TMP="$test_tmp" RP_PROVIDER="self-test" RP_SAMPLES="$samples" RP_KEEP_GOING=1 RP_IDLE_PROBE=0 RP_LOCAL_OPEN_MS=0 RP_REMOTE_OPEN_MS=0 RP_PROVIDER_QUIRK="" RP_LOCAL_ARGV_JSON="$self_argv_json" RP_REMOTE_ARGV_JSON="$self_argv_json" RP_REMOTE_WORKDIR_EXPR="\$HOME/workspace" RP_ENV_NAME="self-test"
  set +e
  python3 -I test/remote_parity.py
  status=$?
  set -e
  self_test_cleanup "$status"
}

if [ "$self_test" -eq 1 ]; then
  self_test_run
fi

# This gate is before every command that could touch state or the network.
if [ -z "$env_name" ] && [ "$accept" -eq 0 ]; then
  print_plan
  exit 0
fi
missing=""
[ -n "$env_name" ] || missing="$missing --env"
[ "$accept" -eq 1 ] || missing="$missing --i-accept-spend"
if [ -n "$missing" ]; then
  print_plan
  echo "refusing: missing$missing" >&2
  exit 2
fi
if [ "${THEGN_CHANNEL:-}" != "dev" ]; then
  echo "refusing: THEGN_CHANNEL=dev is required" >&2
  exit 2
fi

PREFLIGHT_PY='
import json, os, sys
env = os.environ["PRE_ENV"]
try:
    doc = json.loads(sys.stdin.read())
except ValueError as exc:
    print("FAIL P00: doctor --json did not print JSON: " + str(exc), file=sys.stderr)
    sys.exit(1)
rows = doc.get("remote_live") if isinstance(doc, dict) else None
if rows is None:
    print("FAIL P00: doctor --json has no remote_live key", file=sys.stderr)
    sys.exit(1)
row = next((r for r in rows if isinstance(r, dict) and r.get("env") == env), None)
if row is None:
    print("FAIL P00: doctor remote_live has no row for env " + env, file=sys.stderr)
    sys.exit(1)
def show(value):
    if isinstance(value, bool): return "yes" if value else "no"
    if isinstance(value, list): return ",".join(show(x) for x in value) or "none"
    if isinstance(value, dict): return ",".join(k + "=" + show(v) for k, v in sorted(value.items())) or "none"
    return "none" if value in (None, "") else str(value)
if row.get("token_resolves") is not True:
    print("FAIL P00: the token for env " + env + " does not resolve", file=sys.stderr)
    sys.exit(1)
provider = row.get("provider")
if not isinstance(provider, str) or not provider:
    print("FAIL P00: doctor remote_live row has no provider", file=sys.stderr)
    sys.exit(1)
print("INFO P00: credentials entering the sandbox: forwarded_env=" + show(row.get("forwarded_env")) + "; git_token=" + show(row.get("git_token")) + "; login_upload=" + show(row.get("login_upload")))
unbound = row.get("unbound") or []
if unbound: print("WARN P00: unbound ledger sandboxes: " + ", ".join(show(x) for x in unbound) + ": pre-existing sandboxes this run will not touch")
print("PROVIDER=" + provider)
print("PASS P00: preflight")
'

doctor_json="$("$THEGN_BIN" doctor --json 2>/dev/null)" || true
preflight=""
if ! preflight="$(printf '%s' "$doctor_json" | PRE_ENV="$env_name" python3 -I -c "$PREFLIGHT_PY")"; then
  printf '%s\n' "$preflight" >&2
  exit 1
fi
printf '%s\n' "$preflight"
provider="$(printf '%s\n' "$preflight" | sed -n 's/^PROVIDER=//p')"

# Provider-specific code lives in this table. The live parity bridge currently
# supports Sprites; Vercel remains explicit until THE-2234 lands.
provider_ledger_name() {
  case "$provider" in
    sprites)
      local f id state kind
      for f in "$XDG_STATE_HOME"/thegn/vps/*.json; do
        [ -f "$f" ] || continue
        kind="$(sed -n 's/.*"provider"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$f" | head -n 1)"
        state="$(sed -n 's/.*"state"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$f" | head -n 1)"
        [ "$kind" = sprites ] && [ "$state" = ready ] || continue
        id="${f##*/}"
        printf '%s\n' "${id%.json}"
      done
      ;;
    *) return 2 ;;
  esac
}
provider_ledger_all() {
  case "$provider" in
    sprites)
      local f id kind
      for f in "$XDG_STATE_HOME"/thegn/vps/*.json; do
        [ -f "$f" ] || continue
        kind="$(sed -n 's/.*"provider"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$f" | head -n 1)"
        [ "$kind" = sprites ] || continue
        id="${f##*/}"
        printf '%s\n' "${id%.json}"
      done
      ;;
    *) return 2 ;;
  esac
}
provider_session_prefix() {
  case "$provider" in
    sprites) PROVIDER_PREFIX=("$THEGN_BIN" sprite-exec "$1" --) ;;
    *) return 2 ;;
  esac
}
provider_workdir_expr() {
  case "$provider" in
    sprites) printf '%s\n' '$HOME/workspace' ;;
    *) return 2 ;;
  esac
}
provider_known_quirks() {
  case "$provider" in
    sprites) printf '%s\n' 'THE-2450 (first tty chunk without its line ending)' ;;
    *) return 2 ;;
  esac
}
case "$provider" in
  sprites) ;;
  *) echo "remote-parity: no headless exec bridge for provider $provider yet" >&2; exit 2 ;;
esac

pinned_socket="$("$THEGN_BIN" config get daemon.socket 2>/dev/null || true)"
if [ -n "$(printf '%s' "$pinned_socket" | tr -d '[:space:]')" ]; then
  echo "refusing: daemon.socket is pinned to '$pinned_socket'" >&2
  exit 2
fi

TMP="$(mktemp -d)"
export TMP
export XDG_STATE_HOME="$TMP/state" THEGN_DIR="$TMP/thegn"
unset THEGN_CONFIG_DIR THEGN_CONTROL_ADDR THEGN_CONTROL_TOKEN
mkdir -p "$XDG_STATE_HOME" "$THEGN_DIR" "$TMP/repo"
REPO="$TMP/repo"
DAEMON_PID=""
LOCAL_SESSION=""
REMOTE_SESSION=""
RUN_RECORDS=""
ENV_DOWN=0
TEARDOWN_LEFT=0

listing_lacks_name() {
  local name="$1" out
  out="$("$THEGN_BIN" fleet list --env "$env_name" --json 2>/dev/null)" || return 1
  printf '%s' "$out" | LISTED_NAME="$name" python3 -I -c '
import json, os, sys
try:
    doc = json.load(sys.stdin)
    view = doc["view"]
    rows = view["rows"]
except (KeyError, TypeError, ValueError):
    sys.exit(1)
if view.get("partial") or not isinstance(rows, list):
    sys.exit(1)
sys.exit(1 if any(isinstance(r, dict) and r.get("id") == os.environ["LISTED_NAME"] for r in rows) else 0)
'
}

teardown() {
  local rc=$?
  trap - EXIT INT TERM
  set +e
  RUN_RECORDS="$(provider_ledger_all 2>/dev/null || true)"
  [ -z "$LOCAL_SESSION" ] || "$THEGN_BIN" session close "$LOCAL_SESSION" --json >/dev/null 2>&1 || true
  [ -z "$REMOTE_SESSION" ] || "$THEGN_BIN" session close "$REMOTE_SESSION" --json >/dev/null 2>&1 || true
  if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    "$THEGN_BIN" daemon stop >/dev/null 2>&1 || true
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  if [ "$ENV_DOWN" -eq 0 ] && [ -d "$REPO/.git" ]; then
    (cd "$REPO" && "$THEGN_BIN" env down --force) >/dev/null 2>&1 || rc=1
  fi
  local n
  while IFS= read -r n; do
    [ -n "$n" ] || continue
    case "$n" in *[!a-zA-Z0-9._-]*) continue ;; esac
    "$THEGN_BIN" env deprovision "$n" --env "$env_name" >/dev/null 2>&1 || rc=1
  done <<<"$RUN_RECORDS"
  "$THEGN_BIN" env reap --provider "$provider" --apply --yes >/dev/null 2>&1 || rc=1
  while IFS= read -r n; do
    [ -z "$n" ] && continue
    if [ -f "$XDG_STATE_HOME/thegn/vps/$n.json" ] || ! listing_lacks_name "$n"; then
      TEARDOWN_LEFT=1
      rc=1
      echo "TEARDOWN INCOMPLETE: sandbox $n may still exist; keeping $TMP" >&2
    fi
  done <<<"$RUN_RECORDS"
  local leftover
  leftover="$(provider_ledger_all 2>/dev/null || true)"
  if [ -n "$(printf '%s\n' "$leftover" | sed '/^$/d')" ]; then
    TEARDOWN_LEFT=1
    rc=1
    while IFS= read -r n; do
      [ -z "$n" ] || echo "TEARDOWN INCOMPLETE: sandbox $n may still exist; keeping $TMP" >&2
    done <<<"$leftover"
  fi
  if [ "$TEARDOWN_LEFT" -eq 0 ]; then
    rm -rf "$TMP"
  else
    echo "-- keeping $TMP for a later reap" >&2
  fi
  exit "$rc"
}
trap teardown EXIT INT TERM

scratch_repo() {
  git -C "$REPO" init -q -b main &&
    git -C "$REPO" -c user.name=rp -c user.email=rp@example.invalid -c commit.gpgsign=false commit -q --allow-empty -m init &&
    git -C "$REPO" switch -q -c rp/parity
}

print_plan
echo
echo "== About to create: 1 $provider sandbox via env '$env_name' =="
echo "== temp state: $TMP =="
scratch_repo
(cd "$REPO" && "$THEGN_BIN" env set "$env_name" >/dev/null)
export THEGN_LOG="thegn::startup=info" THEGN_TRACE_FILE="$TMP/up.log"
(cd "$REPO" && "$THEGN_BIN" env up) > >(tee "$TMP/up.out") 2>&1
RUN_RECORDS="$(provider_ledger_name)"
if [ "$(printf '%s\n' "$RUN_RECORDS" | sed '/^$/d' | wc -l)" -ne 1 ]; then
  echo "FAIL P00: expected exactly one ready $provider ledger record from this run" >&2
  exit 1
fi
echo "sandbox: $RUN_RECORDS"

"$THEGN_BIN" daemon >"$TMP/daemon.log" 2>&1 &
DAEMON_PID=$!
daemon_ready=0
for _ in $(seq 1 30); do
  if "$THEGN_BIN" session list --json >"$TMP/sessions.json" 2>/dev/null; then
    daemon_ready=1
    break
  fi
  sleep 0.5
done
if [ "$daemon_ready" -ne 1 ]; then
  echo "FAIL P00: daemon did not answer session list within 15 s" >&2
  exit 1
fi

open_params() {
  OPEN_ARGV_JSON="$1" OPEN_CWD="$REPO" python3 -I -c '
import json, os
print(json.dumps({"argv":json.loads(os.environ["OPEN_ARGV_JSON"]),"cwd":os.environ["OPEN_CWD"],"env":[["PS1","PARITY$ "]],"rows":40,"cols":120}, separators=(",", ":")))
'
}
json_argv() { python3 -I -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "$@"; }
session_id() { python3 -I -c 'import json,sys; print(json.load(sys.stdin)["id"])'; }
mono_ns() { python3 -I -c 'import time; print(time.monotonic_ns())'; }

local_argv_json="$(json_argv bash --norc --noprofile -i)"
open_start="$(mono_ns)"
local_open_params="$(open_params "$local_argv_json")"
local_open_json="$("$THEGN_BIN" api call sessions.open --params "$local_open_params")"
open_end="$(mono_ns)"
LOCAL_OPEN_MS="$(( (open_end - open_start) / 1000000 ))"
LOCAL_SESSION="$(printf '%s' "$local_open_json" | session_id)"

provider_session_prefix "$RUN_RECORDS"
remote_cmd='export PS1="PARITY$ "; cd "$HOME/workspace" 2>/dev/null; command -v bash >/dev/null 2>&1 && exec bash --norc --noprofile -i; exec sh -i'
remote_argv_json="$(PROVIDER_PREFIX_JSON="$(json_argv "${PROVIDER_PREFIX[@]}")" REMOTE_CMD="$remote_cmd" python3 -I -c '
import json, os
print(json.dumps(json.loads(os.environ["PROVIDER_PREFIX_JSON"])+["/bin/sh","-c",os.environ["REMOTE_CMD"]]))
')"
open_start="$(mono_ns)"
remote_open_params="$(open_params "$remote_argv_json")"
remote_open_json="$("$THEGN_BIN" api call sessions.open --params "$remote_open_params")"
open_end="$(mono_ns)"
REMOTE_OPEN_MS="$(( (open_end - open_start) / 1000000 ))"
REMOTE_SESSION="$(printf '%s' "$remote_open_json" | session_id)"

# P00: a remote shell that exits inside ten seconds proves the provider token
# never reached the daemon's session environment.
remote_exited=0
for _ in $(seq 1 20); do
  if "$THEGN_BIN" session list --json 2>/dev/null | SESSION_ID="$REMOTE_SESSION" python3 -I -c '
import json, os, sys
try:
    rows = json.load(sys.stdin)
except ValueError:
    sys.exit(1)
if isinstance(rows, dict): rows = rows.get("sessions", [])
for row in rows:
    if isinstance(row, dict) and row.get("id") == os.environ["SESSION_ID"]:
        sys.exit(0 if row.get("exited_at_ms") is not None or row.get("exit_code") is not None else 1)
sys.exit(1)
'; then
    remote_exited=1
    break
  fi
  sleep 0.5
done
if [ "$remote_exited" -eq 1 ]; then
  echo "FAIL P00: the provider token is not visible inside daemon sessions (keyring, or add its name to [sandbox] host_env_allow); see the session's last screen" >&2
  "$THEGN_BIN" session snapshot --session "$REMOTE_SESSION" --text 2>/dev/null | python3 -I -c '
import re, sys
s = sys.stdin.read()
s = re.sub(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[PX^_][^\x1b]*(?:\x1b\\))", "", s)
sys.stdout.write(s)
' >&2 || true
  exit 1
fi

export RP_THEGN_BIN="$THEGN_BIN" RP_LOCAL_SESSION="$LOCAL_SESSION" RP_REMOTE_SESSION="$REMOTE_SESSION" RP_REPO="$REPO" RP_TMP="$TMP" RP_PROVIDER="$provider" RP_SAMPLES="$samples" RP_KEEP_GOING="$keep_going" RP_IDLE_PROBE="$idle_probe" RP_LOCAL_OPEN_MS="$LOCAL_OPEN_MS" RP_REMOTE_OPEN_MS="$REMOTE_OPEN_MS" RP_PROVIDER_QUIRK="$(provider_known_quirks)" RP_LOCAL_ARGV_JSON="$local_argv_json" RP_REMOTE_ARGV_JSON="$remote_argv_json" RP_REMOTE_WORKDIR_EXPR="$(provider_workdir_expr)" RP_ENV_NAME="$env_name"
python3 -I test/remote_parity.py
