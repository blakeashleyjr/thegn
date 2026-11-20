#!/usr/bin/env bash
# Daily live remote run (b162 rx-live): headless agent + pull + land + reap on a
# provider env (sprites, fly or vercel). SPENDS REAL MONEY when acting; with no
# flags it only prints the plan and exits 0 without calling thegn or curl.
#
# Not in ci/ci-local/smoke: runs only through `just remote-daily`. Reads the
# owner's REAL config read-only (secrets are never read or printed) with a fresh
# XDG_STATE_HOME and THEGN_DIR (the login runtime dir is kept so sandbox masks
# cover every instance's control socket; daemon sockets are hashed per state
# dir), and a scratch repo on branch rd/daily-lane.
#
# Usage:
#   test/remote-daily.sh                                   # print plan, no network
#   THEGN_CHANNEL=dev test/remote-daily.sh --env <name> --i-accept-spend \
#     [--provider sprites|fly|vercel] [--with-model] [--with-adopt <sprite>] \
#     [--with-reaper] [--with-pool] [--keep-going]
set -euo pipefail

THEGN_BIN="${THEGN_BIN:-thegn}"
# The scratch repository has no build: the owner's merge gate command must not decide these lands.
# The value is TOML: a bare true is a boolean and is refused, so the string is quoted.
LAND_SET=(--set 'merge_queue.gate_command="true"')

env_name="" accept=0 provider="sprites" with_model=0 adopt_name="" with_reaper=0 with_pool=0 keep_going=0
while [ $# -gt 0 ]; do
  case "$1" in
  --env)
    env_name="${2:?--env needs a value}"
    shift 2
    ;;
  --provider)
    provider="${2:?--provider needs a value}"
    case "$provider" in sprites | fly | vercel) ;; *)
      echo "--provider must be sprites, fly or vercel" >&2
      exit 2
      ;;
    esac
    shift 2
    ;;
  --with-model)
    with_model=1
    shift
    ;;
  --with-adopt)
    adopt_name="${2:?--with-adopt needs a sprite name}"
    case "$adopt_name" in '' | *[!a-z0-9-]*)
      echo "--with-adopt name must match [a-z0-9-]+" >&2
      exit 2
      ;;
    esac
    shift 2
    ;;
  --with-reaper)
    with_reaper=1
    shift
    ;;
  --with-pool)
    with_pool=1
    shift
    ;;
  --i-accept-spend)
    accept=1
    shift
    ;;
  --keep-going)
    keep_going=1
    shift
    ;;
  -h | --help)
    sed -n 2,15p "$0"
    exit 0
    ;;
  *)
    echo "unknown argument: $1" >&2
    exit 2
    ;;
  esac
done
# shellcheck disable=SC2034 # read by test/lib/live.sh
LIVE_KEEP_GOING="$keep_going"

if [ "$with_pool" = 1 ] && [ "$provider" != sprites ]; then
  echo "--with-pool is supported only with --provider sprites" >&2
  exit 2
fi

print_plan() {
  cat <<PLAN
remote-daily plan (nothing is created without both --env <name> and
--i-accept-spend, plus THEGN_CHANNEL=dev)

Will create : ONE sandbox/machine via the named [env.<name>] from your real
              config (read-only), in a fresh XDG_STATE_HOME and THEGN_DIR (the
              login runtime dir is kept so sandbox masks cover every instance's
              control socket; daemon sockets are hashed per state dir), with
              a scratch git repo on branch rd/daily-lane.
Max spend   : one sandbox for about 10 minutes (a 420 s scripted hold); more
              with --with-model (two real model runs) or --with-pool (one
              warm spare). Teardown destroys it.
Options     : --keep-going alone is plan-only and does not bypass
              --i-accept-spend; with it, the run continues and prints
              FAILED STEPS: <ids> before exiting non-zero. --with-pool is
              sprites-only and runs the opt-in P steps below.
Teardown    : trap on EXIT/INT/TERM -> env down --force, env deprovision <name>
              for each name in THIS run's ledger (rd-x's, rd-y's and rd-p's), daemon
              stop; fails (keeps the ledger) if any of this run's names remain
              in the ledger or the org listing.
The preflight prints which credentials enter the sandbox.
Steps:
  0   preflight: doctor --json remote_live for <S>; FAIL if the token does not
      resolve; INFO forwarded_env, git_token, login_upload; WARN lists unbound
      pre-existing sandboxes (never touched); SKIP on an older binary or fly
  1   env test
  2   wt new rd-x --env <S>
 2c   resolve the rd-x worktree path
 2u   env up (provision rd-x)
 2b   exactly one ready ledger record for this run
  3   agent run --worktree <rd-x path> --agent hold-test --prompt x --wait --pull --json
      (scripted agent: sleep 420; date>f; git commit). With RD_OVERLAY_AGENT=1,
      a temporary copied config supplies hold-test; otherwise the stated
      fallback uses the exec bridge (sprite-exec --hold / vercel-exec) followed
      by env pull. If your config defines a hold-test agent, set RD_HOLD_AGENT=1
      to use agent run instead; RD_OVERLAY_AGENT=1 also supplies one for the run.
  4   (--with-model) agent run --agent claude, then --agent codex, each
      committing one file
  5   (fly only) fleet list --env shows stopped
  6   if login_upload=claude, exec bridge stat -c %a ~/.claude/.credentials.json is 600;
      otherwise no model login entered the sandbox (model_credentials default)
  7   local git log has the commits
  8   land --worktree <rd-x path> --release-sandbox
      (requires the land to report "released sandbox <name>")
       (both lands pass --set merge_queue.gate_command="true": the scratch repository has no build)
  9   ledger record gone, sandbox absent from the org listing
 10   env reap --provider <p> --json is clean
 11   wt new rd-y --env <S>
11p   resolve the rd-y worktree path
11u   env up (provision rd-y)
11b   exactly one new ledger record
11m   (sprites/vercel only) commit inside rd-y through the exec bridge
11k   (sprites/vercel only) land rd-y without --release-sandbox (sandbox kept)
      (requires "pulled tg/rd-y" and the sandbox commit in main)
11j   (sprites/vercel only) rd-y's record is still ready and the listing has it
11o   leave an uncommitted file inside rd-y
11g   land --release-sandbox refuses the dirty sandbox and keeps it
11e   env down refuses the dirty sandbox and leaves its record ready
11f   remove the uncommitted file
11n   commit inside rd-y again, left unpulled
11d   env deprovision of rd-y's sandbox refuses while it belongs to the worktree
11r   wt rm <rd-y path> --force exits 0 and pulls tg/rd-y
11q   local tg/rd-y has the second sandbox commit
11c   its record is gone and the org listing lacks it
 P0  (--with-pool) wt new rd-p --env <S>, then resolve its worktree path
 P1  (--with-pool) snapshot the ledger, env pool-fill --worktree <rd-p>
     --size 1 --json, and verify exactly one filled spare
 P2  (--with-pool) exactly one new ledger record is the filled spare
 P3  (--with-pool) claims the spare without a new ledger record: RD_HOLD_AGENT=1
     uses agent run and otherwise env up --worktree <rd-p path>
 P4  (--with-pool) env pool-fill --worktree <rd-p> --size 0 --json
 P5  (--with-pool) wt rm <rd-p> --force, then verify the spare is gone
 A    (--with-adopt <name>) env adopt <name> --env <S> --yes; ledger record
      is checked, then that record file is removed: the adopted sprite was
      not created by this run and is NEVER destroyed by this script
 R    (--with-reaper) lifetime_reap on a throwaway env, hydrate cadence
Each step prints PASS/FAIL; the first FAIL tears down and exits non-zero.
With --keep-going, failed steps print FAIL <id>, the run continues, and the
end prints FAILED STEPS: <ids> before exiting non-zero. ALL STEPS PASS prints
only when no step failed.
Each step also prints TIME <id> <seconds> s; the end prints STARTUP lines (start-up numbers).
PLAN
}

# shellcheck source=test/lib/live.sh disable=SC1091
. "$(dirname "${BASH_SOURCE[0]}")/lib/live.sh"
# ---- flag gate: nothing above this line touches the network or thegn ----
if [ -z "$env_name" ] && [ "$accept" = 0 ]; then
  print_plan
  exit 0
fi
missing=""
[ -n "$env_name" ] || missing="$missing --env"
[ "$accept" = 1 ] || missing="$missing --i-accept-spend"
if [ -n "$missing" ]; then
  print_plan
  echo "refusing: missing${missing}" >&2
  exit 2
fi
if [ "${THEGN_CHANNEL:-}" != "dev" ]; then
  echo "refusing: THEGN_CHANNEL=dev is required" >&2
  exit 2
fi

if [ "$provider" = fly ] && [ "${RD_HOLD_AGENT:-0}" != 1 ] && [ "${RD_OVERLAY_AGENT:-0}" != 1 ]; then
  echo "fly needs RD_HOLD_AGENT=1 (no exec bridge)" >&2
  exit 2
fi
# ---- step 0: preflight (doctor's "Remote live readiness" block) ----
preflight_capture() { live_preflight_capture; }
preflight_check() { live_preflight_check "$1" "${2:-1}" "$TMP"; }
# Run against the owner's real state, before the fresh dirs below, so
# pre-existing unbound sandboxes are listed.
preflight_capture
OVERLAY_OWNER_CONFIG=""
if [ "${RD_OVERLAY_AGENT:-0}" = 1 ]; then
  OVERLAY_OWNER_CONFIG="$("$THEGN_BIN" config path 2>/dev/null)" || OVERLAY_OWNER_CONFIG=""
fi

TMP="$(mktemp -d)"
export XDG_STATE_HOME="$TMP/state" THEGN_DIR="$TMP/thegn"
unset THEGN_CONFIG_DIR
mkdir -p "$XDG_STATE_HOME" "$THEGN_DIR" "$TMP/repo"
REPO="$TMP/repo"
NAME="(unknown)"
CREATED=0
# shellcheck disable=SC2034 # appended to and read by test/lib/live.sh
FAILED_STEPS=()
HOLD_PID=""
DAEMON_PID=""
WT_PATH=""
RDY_NAME=""
RDY_PATH=""
POOL_SPARE=""
POOL_PATH=""
OVERLAY_CONFIG=""
CONFIG_ARGS=()
# shellcheck disable=SC2034 # read by test/lib/live.sh
LIVE_STARTUP=("2u=cold create (env up rd-x)" "11u=second create (env up rd-y)" "P1=warm spare fill (env pool-fill)" "P3=claim the warm spare")
case "$provider" in
sprites) EXEC_BRIDGE="sprite-exec" ;;
vercel) EXEC_BRIDGE="vercel-exec" ;;
*) EXEC_BRIDGE="" ;; # fly has no thegn exec bridge verb
esac

setup_overlay() {
  [ "${RD_OVERLAY_AGENT:-0}" = 1 ] || return 0
  if [ -z "$OVERLAY_OWNER_CONFIG" ]; then
    echo "SKIP overlay: thegn config path failed" >&2
    return 0
  fi
  if [ ! -f "$OVERLAY_OWNER_CONFIG" ]; then
    echo "SKIP overlay: owner config is not a file: $OVERLAY_OWNER_CONFIG" >&2
    return 0
  fi
  OVERLAY_CONFIG="$TMP/config.toml"
  if ! cp -- "$OVERLAY_OWNER_CONFIG" "$OVERLAY_CONFIG"; then
    echo "SKIP overlay: could not copy the owner config" >&2
    OVERLAY_CONFIG=""
    return 0
  fi
  if ! cat >>"$OVERLAY_CONFIG" <<'OVERLAY'; then

[[agents]]
name = "hold-test"
command = "sleep 420 && date > rd-hold.txt && git add rd-hold.txt && git -c user.name=rd -c user.email=rd@example.invalid -c commit.gpgsign=false commit -q -m rd-hold && :"
OVERLAY
    echo "SKIP overlay: could not append hold-test to the copied config" >&2
    OVERLAY_CONFIG=""
    return 0
  fi
  local out rc
  out="$("$THEGN_BIN" --config "$OVERLAY_CONFIG" agent list 2>&1)" && rc=0 || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "SKIP overlay: agent list failed: $out" >&2
    OVERLAY_CONFIG=""
    return 0
  fi
  case "$out" in
  *hold-test*) ;;
  *)
    echo "SKIP overlay: agent list did not name hold-test" >&2
    OVERLAY_CONFIG=""
    return 0
    ;;
  esac
  CONFIG_ARGS=(--config "$TMP/config.toml")
}

setup_overlay
if [ "$provider" = fly ] && [ "${RD_HOLD_AGENT:-0}" != 1 ] && [ -z "$OVERLAY_CONFIG" ]; then
  echo "fly needs RD_HOLD_AGENT=1 or a working RD_OVERLAY_AGENT=1 (no exec bridge)" >&2
  exit 2
fi

print_plan
echo
echo "== About to create: 1 $provider sandbox via env '$env_name' =="
echo "== temp state: $TMP =="

ledger_dir() { printf '%s\n' "$XDG_STATE_HOME/thegn/vps"; }
run_records() { # names of this run's ledger records for $provider
  local f
  for f in "$(ledger_dir)"/*.json; do
    [ -f "$f" ] || continue
    if grep -Eq "\"provider\": *\"$provider\"" "$f"; then basename "$f" .json; fi
  done
  return 0
}

listing_state() { # listing_state <name>: prints absent | present | unknown
  local out
  out="$("$THEGN_BIN" fleet list --env "$env_name" --json 2>/dev/null)" || {
    echo unknown
    return 0
  }
  printf '%s' "$out" | LISTED_NAME="$1" python3 -I -c '
import json, os, sys
try:
    view = json.load(sys.stdin)["view"]
except Exception:
    print("unknown"); sys.exit(0)
if view.get("partial"):
    print("unknown"); sys.exit(0)
n = os.environ["LISTED_NAME"]
rows = view.get("rows") or []
print("present" if any(r.get("id") == n or r.get("name") == n for r in rows) else "absent")
'
}
listing_lacks_name() { # listing_lacks_name [name]: defaults to rd-x's sandbox; a partial or failed listing is not proof
  local n="${1:-$NAME}" s
  s="$(listing_state "$n")"
  echo "   org listing for $n: $s"
  [ "$s" = absent ]
}
listing_has_name() { # listing_has_name <name>: only a complete listing proves presence
  local out
  out="$("$THEGN_BIN" fleet list --env "$env_name" --json 2>/dev/null)" || return 1
  printf '%s' "$out" | LISTED_NAME="$1" python3 -I -c '
import json, os, sys

try:
    listing = json.load(sys.stdin)
    view = listing["view"]
except (KeyError, TypeError, ValueError):
    print("fleet list JSON is invalid or missing view", file=sys.stderr)
    sys.exit(1)
if not isinstance(view, dict):
    print("fleet list JSON view is not an object", file=sys.stderr)
    sys.exit(1)
if view.get("partial"):
    print("fleet list is partial; cannot prove sandbox presence", file=sys.stderr)
    sys.exit(1)
rows = view.get("rows")
if not isinstance(rows, list):
    print("fleet list JSON view.rows is not an array", file=sys.stderr)
    sys.exit(1)
name = os.environ["LISTED_NAME"]
if not any(isinstance(row, dict) and (row.get("id") == name or row.get("name") == name) for row in rows):
    print("fleet list has no row for " + name, file=sys.stderr)
    sys.exit(1)
print("   org listing contains " + name)
'
}

teardown() {
  local rc=$?
  trap - EXIT INT TERM
  set +e
  local left=0 names n
  if [ -n "$HOLD_PID" ] && kill -0 "$HOLD_PID" 2>/dev/null; then
    pkill -P "$HOLD_PID" 2>/dev/null
    kill "$HOLD_PID" 2>/dev/null
    sleep 2
    kill -0 "$HOLD_PID" 2>/dev/null && {
      echo "TEARDOWN INCOMPLETE: hold process $HOLD_PID survived" >&2
      rc=1
    }
  fi
  if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null
    wait "$DAEMON_PID" 2>/dev/null
    DAEMON_PID=""
  fi
  if [ "$CREATED" = 1 ]; then
    # rd-y's name came from this run's ledger (11b); keep it even if its
    # record is gone, so the listing check below still covers it.
    names="$(run_records)"
    if [ -n "$RDY_NAME" ] && ! printf '%s\n' "$names" | grep -Fxq "$RDY_NAME"; then
      names="$names $RDY_NAME"
    fi
    if [ -n "$POOL_SPARE" ] && ! printf '%s\n' "$names" | grep -Fxq "$POOL_SPARE"; then
      names="$names $POOL_SPARE"
    fi
    for wt in "$RDY_PATH" "$POOL_PATH"; do
      if [ -n "$wt" ] && [ -d "$wt" ]; then
        echo "-- teardown: wt rm $wt --force"
        (cd "$REPO" && "$THEGN_BIN" wt rm "$wt" --force </dev/null)
      fi
    done
    echo "-- teardown: env down --force"
    (cd "$REPO" && "$THEGN_BIN" env down --force)
    for n in $names; do
      [ "$n" = "$adopt_name" ] && continue
      case "$n" in *[!a-z0-9-]*) continue ;; esac
      if [ -f "$(ledger_dir)/$n.json" ]; then
        echo "-- teardown: env deprovision $n"
        "$THEGN_BIN" env deprovision "$n" --env "$env_name" --yes </dev/null
      fi
    done
    for n in $names; do
      [ "$n" = "$adopt_name" ] && continue
      if [ -f "$(ledger_dir)/$n.json" ] || [ "$(listing_state "$n")" != absent ]; then
        left=1
        rc=1
        echo "TEARDOWN INCOMPLETE: $n may still exist; the ledger is kept in $TMP" >&2
      fi
    done
    echo "-- teardown: daemon stop"
    "$THEGN_BIN" daemon stop
  fi
  if [ "$left" = 1 ]; then
    echo "-- keeping $TMP (ledger needed for a later reap)" >&2
    ls "$(ledger_dir)" >&2
  else
    rm -rf "$TMP"
  fi
  exit "$rc"
}
trap teardown EXIT INT TERM

step() { # step <id> <description> <command...>
  live_step "$@"
}

in_repo() { (cd "$REPO" && "$@"); }
no_stdin() { "$@" </dev/null; }
thegn_cli() { "$THEGN_BIN" "${CONFIG_ARGS[@]}" "$@"; }

LOGIN_UPLOAD=""
read_login_upload() {
  [ -f "$TMP/preflight.env" ] || return 0
  LOGIN_UPLOAD="$(sed -n 's/^LOGIN_UPLOAD=//p' "$TMP/preflight.env")"
}

name_from_ledger() { # exactly one ready record, safe charset
  local n
  n="$(run_records)"
  [ -n "$n" ] && [ "$(printf '%s\n' "$n" | wc -l)" -eq 1 ] || return 1
  case "$n" in *[!a-z0-9-]*) return 1 ;; esac
  grep -Eq '"state": *"ready"' "$(ledger_dir)/$n.json" || return 1
  NAME="$n"
}

scratch_repo() {
  git -C "$REPO" init -q -b main &&
    git -C "$REPO" -c user.name=rd -c user.email=rd@example.invalid -c commit.gpgsign=false \
      commit -q --allow-empty -m init &&
    git -C "$REPO" switch -q -c rd/daily-lane
}

hold_script_fallback() { # exec-bridge fallback for the scripted hold-test agent
  [ -n "$EXEC_BRIDGE" ] || return 1
  local hold=()
  # Only sprite-exec has --hold; vercel-exec takes none.
  [ "$provider" = sprites ] && hold=(--hold 10m)
  # shellcheck disable=SC2016 # $HOME must expand inside the sandbox
  no_stdin "$THEGN_BIN" "$EXEC_BRIDGE" "${hold[@]}" "$NAME" -- sh -c \
    'cd "$HOME/workspace" && sleep 420 && date > rd-hold.txt && git add rd-hold.txt && git -c user.name=rd -c user.email=rd@example.invalid -c commit.gpgsign=false commit -q -m rd-hold' &
  HOLD_PID=$!
}
hold_wait() {
  local pid="$HOLD_PID"
  wait "$pid" || return 1
  HOLD_PID=""
}

start_overlay_daemon() {
  [ -n "$OVERLAY_CONFIG" ] || return 0
  "$THEGN_BIN" daemon stop >/dev/null 2>&1 || true
  no_stdin "$THEGN_BIN" --config "$OVERLAY_CONFIG" daemon >"$TMP/overlay-daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 30); do
    "$THEGN_BIN" --config "$OVERLAY_CONFIG" session list --json >/dev/null 2>&1 && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || break
    sleep 0.5
  done
  echo "overlay daemon exited before step 3" >&2
  sed -n '1,40p' "$TMP/overlay-daemon.log" >&2
  return 1
}

agent_json_ok() { # agent_json_ok <file>: validate a completed, pulled agent run
  python3 -I -c '
import json, sys

path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as stream:
        result = json.load(stream)
except (OSError, TypeError, ValueError):
    print("agent JSON failing field: json", file=sys.stderr)
    sys.exit(1)

if not isinstance(result, dict):
    print("agent JSON failing field: json", file=sys.stderr)
    sys.exit(1)
checks = [
    ("status", result.get("status"), "exited"),
    ("exit_code", result.get("exit_code"), 0),
]
pull = result.get("pull")
checks.append(("pull.ok", pull.get("ok") if isinstance(pull, dict) else None, True))
for field, actual, expected in checks:
    if (field == "pull.ok" and actual is not True) or (field != "pull.ok" and actual != expected):
        print("agent JSON failing field: " + field, file=sys.stderr)
        sys.exit(1)
' "$1"
}

agent_hold_run() {
  if in_repo thegn_cli agent run --worktree "$WT_PATH" --agent hold-test --prompt x --wait --pull --json >"$TMP/3.json"; then
    agent_json_ok "$TMP/3.json" || return 1
    local provision
    provision="$(python3 -I -c '
import json, sys

try:
    with open(sys.argv[1], encoding="utf-8") as f:
        provision = json.load(f)["provision"]
except (KeyError, OSError, TypeError, ValueError):
    print("agent JSON missing provision", file=sys.stderr)
    sys.exit(1)
if not isinstance(provision, str):
    print("agent JSON provision is not a string", file=sys.stderr)
    sys.exit(1)
print(provision)
' "$TMP/3.json")" || return 1
    echo "INFO provision=$provision"
  else
    local rc=$?
    agent_json_ok "$TMP/3.json" || true
    return "$rc"
  fi
}

pool_fill_one() {
  snapshot_records
  in_repo "$THEGN_BIN" env pool-fill --worktree "$POOL_PATH" --size 1 --json >"$TMP/p1.json" || return 1
  POOL_SPARE="$(python3 -I -c '
import json, re, sys

try:
    with open(sys.argv[1], encoding="utf-8") as stream:
        result = json.load(stream)
except (OSError, TypeError, ValueError):
    print("pool-fill JSON is invalid", file=sys.stderr)
    sys.exit(1)
filled = result.get("filled") if isinstance(result, dict) else None
if not isinstance(filled, list) or len(filled) != 1:
    print("pool-fill JSON must contain exactly one filled name", file=sys.stderr)
    sys.exit(1)
name = filled[0]
if not isinstance(name, str) or re.fullmatch(r"[a-z0-9-]+", name) is None:
    print("pool-fill returned an invalid filled name", file=sys.stderr)
    sys.exit(1)
print(name)
' "$TMP/p1.json")" || return 1
  echo "   filled pool spare: $POOL_SPARE"
}

pool_claim_run() {
  if [ "${RD_HOLD_AGENT:-0}" = 1 ]; then
    if in_repo thegn_cli agent run --worktree "$POOL_PATH" --agent hold-test --prompt x --wait --pull --json >"$TMP/p3.json"; then
      agent_json_ok "$TMP/p3.json" || return 1
      python3 -I -c '
import json, sys

try:
    with open(sys.argv[1], encoding="utf-8") as f:
        if json.load(f)["provision"] != "claimed_spare":
            print("agent JSON provision is not claimed_spare", file=sys.stderr)
            sys.exit(1)
except (KeyError, OSError, TypeError, ValueError):
    print("agent JSON missing provision", file=sys.stderr)
    sys.exit(1)
' "$TMP/p3.json" || return 1
    else
      local rc="$?"
      agent_json_ok "$TMP/p3.json" || true
      return "$rc"
    fi
  else
    local out rc
    if [ -n "$OVERLAY_CONFIG" ]; then
      out="$(no_stdin "$THEGN_BIN" --config "$TMP/config.toml" env up --worktree "$POOL_PATH" 2>&1)" && rc=0 || rc=$?
    else
      out="$(no_stdin "$THEGN_BIN" env up --worktree "$POOL_PATH" 2>&1)" && rc=0 || rc=$?
    fi
    printf '%s\n' "$out"
    [ "$rc" -eq 0 ] || return "$rc"
    case "$out" in
    *"claimed a warm spare for "* | *claimed_spare*) ;;
    *)
      echo "env up did not report a warm-spare claim" >&2
      return 1
      ;;
    esac
  fi
  local after
  after="$(run_records)"
  echo "   ledger records after pool claim: ${after:-(none)}"
  if [ "$after" = "$RECORDS_BEFORE" ]; then
    return 0
  fi
  return 1
}

pool_new_record() {
  if [ -n "$POOL_SPARE" ]; then
    if one_new_record "$POOL_SPARE"; then
      return 0
    fi
    return 1
  fi
  return 1
}

pool_remove() {
  no_stdin in_repo "$THEGN_BIN" wt rm "$POOL_PATH" --force || return 1
  record_gone "$POOL_SPARE" || return 1
  listing_lacks_name "$POOL_SPARE"
}

agent_run() { # agent_run <agent> ; each agent commits one file
  local json_id
  case "$1" in
  claude) json_id=4a ;;
  codex) json_id=4b ;;
  *) return 2 ;;
  esac
  if in_repo "$THEGN_BIN" agent run --worktree "$WT_PATH" --agent "$1" --prompt "create $1-rd.txt containing ok and commit it" --wait --pull --json >"$TMP/$json_id.json"; then
    agent_json_ok "$TMP/$json_id.json"
  else
    local rc=$?
    agent_json_ok "$TMP/$json_id.json" || true
    return "$rc"
  fi
}

fly_stopped() {
  local out
  out="$("$THEGN_BIN" fleet list --env "$env_name" --json)" || return 1
  printf '%s' "$out" | FLEET_ID="$NAME" python3 -I -c '
import json, os, sys

try:
    listing = json.load(sys.stdin)
    view = listing["view"]
    rows = view["rows"]
except (KeyError, TypeError, ValueError):
    print("fleet list JSON is invalid or missing view.rows", file=sys.stderr)
    sys.exit(1)
if not isinstance(view, dict) or not isinstance(rows, list):
    print("fleet list JSON view.rows is not an array", file=sys.stderr)
    sys.exit(1)
name = os.environ["FLEET_ID"]
row = next((item for item in rows if isinstance(item, dict) and item.get("id") == name), None)
if row is None:
    print("fleet list has no row for " + name, file=sys.stderr)
    sys.exit(1)
state = row.get("state")
if not isinstance(state, str) or state.lower() not in ("stopped", "suspended"):
    print("fleet row " + name + " is not stopped: " + repr(state), file=sys.stderr)
    sys.exit(1)
print("   fleet truth: " + name + " is " + state)
'
}

cred_mode() {
  [ -n "$EXEC_BRIDGE" ] || return 0
  local out
  # shellcheck disable=SC2016 # $HOME expands inside the sandbox
  out="$(no_stdin "$THEGN_BIN" "$EXEC_BRIDGE" "$NAME" -- sh -c 'stat -c %a "$HOME/.claude/.credentials.json"')" || return 1
  out="$(printf '%s' "$out" | tr -d '\r\n ')"
  echo "   mode: $out"
  [ "$out" = 600 ]
}

no_model_login() {
  [ -n "$EXEC_BRIDGE" ] || return 0
  local out
  # shellcheck disable=SC2016 # $HOME must expand inside the sandbox
  out="$(no_stdin "$THEGN_BIN" "$EXEC_BRIDGE" "$NAME" -- sh -c 'test ! -e "$HOME/.claude/.credentials.json" && echo absent')" || return 1
  out="$(printf '%s' "$out" | tr -d '\r\n ')"
  echo "   credentials: $out"
  [ "$out" = absent ]
}

local_log_has_commits() {
  local n want="${1:-1}"
  n="$(git -C "$WT_PATH" rev-list --count main..HEAD 2>/dev/null || echo 0)"
  echo "   commits ahead of main: $n"
  [ "$n" -ge "$want" ]
}

record_gone() { [ ! -f "$(ledger_dir)/${1:-$NAME}.json" ]; }
rdy_gone() { record_gone "$RDY_NAME" && listing_lacks_name "$RDY_NAME"; }

RECORDS_BEFORE=""
snapshot_records() { RECORDS_BEFORE="$(run_records)"; }
one_new_record() { # exactly one ready ledger record that was not there before
  local expected="${1:-}" n new=""
  for n in $(run_records); do
    printf '%s\n' "$RECORDS_BEFORE" | grep -Fxq "$n" || new="$new $n"
  done
  new="${new# }"
  echo "   new ledger records: ${new:-(none)}"
  [ -n "$new" ] || return 1
  case "$new" in *' '* | *[!a-z0-9-]*) return 1 ;; esac
  [ -z "$expected" ] || [ "$new" = "$expected" ] || return 1
  grep -Eq '"state": *"ready"' "$(ledger_dir)/$new.json" || return 1
  [ -n "$expected" ] || RDY_NAME="$new"
}

reap_clean() {
  local out
  out="$("$THEGN_BIN" env reap --provider "$provider" --json 2>&1)" || return 1
  printf '%s\n' "$out" | head -40
  ! printf '%s' "$out" | grep -Fq "\"$NAME\""
}

adopt_step() {
  "$THEGN_BIN" env adopt "$adopt_name" --env "$env_name" --yes --json &&
    [ -f "$(ledger_dir)/$adopt_name.json" ] || return 1
  # Not created by this run: drop the record so no later command (teardown,
  # reap, env down) can destroy the adopted sprite.
  rm -f "$(ledger_dir)/$adopt_name.json"
}

wt_path_for() { # wt_path_for <suffix>: the worktree whose branch ends in <suffix>
  git -C "$REPO" worktree list --porcelain |
    awk -v s="$1" '/^worktree /{p=substr($0,10)} /^branch / && substr($0, length($0) - length(s) + 1) == s {print p}'
}
resolve_wt_path() { # the worktree whose branch ends in rd-x
  WT_PATH="$(wt_path_for rd-x)"
  echo "   worktree: ${WT_PATH:-(none)}"
  [ -n "$WT_PATH" ] && [ "$(printf '%s\n' "$WT_PATH" | wc -l)" -eq 1 ]
}
resolve_rdy_path() { # the worktree whose branch ends in rd-y
  RDY_PATH="$(wt_path_for rd-y)"
  echo "   worktree: ${RDY_PATH:-(none)}"
  [ -n "$RDY_PATH" ] && [ "$(printf '%s\n' "$RDY_PATH" | wc -l)" -eq 1 ]
}
resolve_pool_path() { # the worktree whose branch ends in rd-p
  POOL_PATH="$(wt_path_for rd-p)"
  echo "   worktree: ${POOL_PATH:-(none)}"
  [ -n "$POOL_PATH" ] && [ "$(printf '%s\n' "$POOL_PATH" | wc -l)" -eq 1 ]
}
rdy_commit() {
  # shellcheck disable=SC2016 # $HOME must expand inside the sandbox
  no_stdin "$THEGN_BIN" "$EXEC_BRIDGE" "$RDY_NAME" -- sh -c \
    'cd "$HOME/workspace" && git -c user.name=rd -c user.email=rd@example.invalid -c commit.gpgsign=false commit --allow-empty -qm rd-y'
}
rdy_dirty_file() {
  # shellcheck disable=SC2016 # $HOME must expand inside the sandbox
  no_stdin "$THEGN_BIN" "$EXEC_BRIDGE" "$RDY_NAME" -- sh -c \
    'cd "$HOME/workspace" && date > rd-dirty.txt'
}
rdy_clean_dirty_file() {
  # shellcheck disable=SC2016 # $HOME must expand inside the sandbox
  no_stdin "$THEGN_BIN" "$EXEC_BRIDGE" "$RDY_NAME" -- sh -c \
    'cd "$HOME/workspace" && rm rd-dirty.txt'
}
rdy_commit_again() {
  # shellcheck disable=SC2016 # $HOME must expand inside the sandbox
  no_stdin "$THEGN_BIN" "$EXEC_BRIDGE" "$RDY_NAME" -- sh -c \
    'cd "$HOME/workspace" && git -c user.name=rd -c user.email=rd@example.invalid -c commit.gpgsign=false commit --allow-empty -qm rd-y2'
}
land_release() {
  local out rc
  out="$(in_repo "$THEGN_BIN" "${LAND_SET[@]}" land --worktree "$WT_PATH" --release-sandbox 2>&1)" && rc=0 || rc=$?
  printf '%s\n' "$out"
  [ "$rc" -eq 0 ] || return "$rc"
  case "$out" in
  *"! --release-sandbox ignored"* | *$'\n'"! sandbox "* | "! sandbox "*) return 1 ;;
  esac
  case "$out" in
  *"released sandbox $NAME"* | *"returned sandbox $NAME"*) ;;
  *)
    echo "land did not report releasing sandbox $NAME" >&2
    return 1
    ;;
  esac
}
rdy_land_keep() {
  local out subjects
  if out="$(in_repo "$THEGN_BIN" "${LAND_SET[@]}" land --worktree "$RDY_PATH" 2>&1)"; then
    printf '%s\n' "$out"
  else
    local rc=$?
    printf '%s\n' "$out"
    return "$rc"
  fi
  case "$out" in
  *"pulled tg/rd-y"*) ;;
  *)
    echo "land did not report pulling tg/rd-y" >&2
    return 1
    ;;
  esac
  subjects="$(git -C "$REPO" log --format=%s main)" || return 1
  case $'\n'"$subjects"$'\n' in
  *$'\n'rd-y$'\n'*) ;;
  *)
    echo "scratch repository main lacks the rd-y commit" >&2
    return 1
    ;;
  esac
  case "$out" in
  *"sandbox $RDY_NAME kept"*) ;;
  *) return 1 ;;
  esac
}
rdy_land_dirty_release() {
  local out rc
  out="$(no_stdin in_repo "$THEGN_BIN" "${LAND_SET[@]}" land --worktree "$RDY_PATH" --release-sandbox 2>&1)" && rc=0 || rc=$?
  printf '%s\n' "$out"
  [ "$rc" -eq 0 ] || return "$rc"
  case "$out" in
  *"sandbox $RDY_NAME kept"*) ;;
  *)
    echo "dirty land did not keep sandbox $RDY_NAME" >&2
    return 1
    ;;
  esac
  case "$out" in
  *uncommitted*) ;;
  *)
    echo "dirty land did not report uncommitted work" >&2
    return 1
    ;;
  esac
}
rdy_down_dirty() {
  local out rc state
  out="$(no_stdin in_repo "$THEGN_BIN" env down --worktree "$RDY_PATH" 2>&1)" && rc=0 || rc=$?
  printf '%s\n' "$out"
  [ "$rc" -ne 0 ] || return 1
  case "$out" in
  *uncommitted*) ;;
  *)
    echo "env down did not report uncommitted work" >&2
    return 1
    ;;
  esac
  state="$(sed -n 's/.*"state": *"\([^"]*\)".*/\1/p' "$(ledger_dir)/$RDY_NAME.json")"
  if [ "$state" = ready ]; then
    return 0
  fi
  return 1
}
rdy_deprovision_bound() {
  local out rc
  out="$(no_stdin in_repo "$THEGN_BIN" env deprovision "$RDY_NAME" 2>&1)" && rc=0 || rc=$?
  printf '%s\n' "$out"
  [ "$rc" -ne 0 ] || return 1
  case "$out" in
  *"belongs to worktree"*) return 0 ;;
  *)
    echo "env deprovision did not report worktree ownership" >&2
    return 1
    ;;
  esac
}
rdy_rm_force() {
  local out rc
  out="$(no_stdin in_repo "$THEGN_BIN" wt rm "$RDY_PATH" --force 2>&1)" && rc=0 || rc=$?
  printf '%s\n' "$out"
  [ "$rc" -eq 0 ] || return "$rc"
  case "$out" in
  *"pulled tg/rd-y"*) return 0 ;;
  *)
    echo "wt rm did not report pulling tg/rd-y" >&2
    return 1
    ;;
  esac
}
rdy_second_commit_landed() {
  local subjects
  subjects="$(git -C "$REPO" log --format=%s tg/rd-y)" || return 1
  case $'\n'"$subjects"$'\n' in
  *$'\n'rd-y2$'\n'*) return 0 ;;
  *)
    echo "local tg/rd-y lacks the second sandbox commit" >&2
    return 1
    ;;
  esac
}
rdy_ready_and_listed() {
  grep -Eq '"state": *"ready"' "$(ledger_dir)/$RDY_NAME.json" && listing_has_name "$RDY_NAME"
}

# fly envs have no remote_live row (doctor covers sprites and vercel).
if [ "$provider" = fly ]; then preflight_check "$env_name" 0; else preflight_check "$env_name"; fi
read_login_upload
step 1 "env test" "$THEGN_BIN" env test "$env_name"
scratch_repo
in_repo "$THEGN_BIN" env set "$env_name" >/dev/null
CREATED=1
step 2 "wt new rd-x --env $env_name" in_repo "$THEGN_BIN" wt new rd-x --env "$env_name"
step 2c "resolve the rd-x worktree path" resolve_wt_path
step 2u "env up (provision rd-x)" no_stdin "$THEGN_BIN" env up --worktree "$WT_PATH"
step 2b "exactly one ready ledger record for this run" name_from_ledger
echo "sandbox: $NAME"
if [ "${RD_HOLD_AGENT:-0}" = 1 ]; then
  step 3d "start the overlay daemon" start_overlay_daemon
  step 3 "agent run hold-test --wait --pull" agent_hold_run
elif [ -n "$OVERLAY_CONFIG" ]; then
  step 3d "start the overlay daemon" start_overlay_daemon
  step 3 "agent run hold-test --wait --pull" agent_hold_run
else
  echo "   (no overlay hold-test agent: using the $EXEC_BRIDGE fallback + env pull)"
  step 3a "scripted hold starts" hold_script_fallback
  step 3b "scripted hold finishes (420 s)" hold_wait
  step 3c "env pull" in_repo "$THEGN_BIN" env pull --worktree "$WT_PATH"
fi
if [ "$with_model" = 1 ]; then
  step 4a "agent run claude commits one file" agent_run claude
  step 4b "agent run codex commits one file" agent_run codex
else
  echo "SKIP 4 (needs --with-model)"
fi
if [ "$provider" = fly ]; then
  step 5 "fleet list shows the machine stopped" fly_stopped
fi
case ",$LOGIN_UPLOAD," in
*,claude,*)
  step 6 "credentials file mode is 600" cred_mode
  ;;
*)
  step 6 "no model login entered the sandbox (model_credentials default)" no_model_login
  ;;
esac
if [ "$with_model" = 1 ]; then want=3; else want=1; fi
step 7 "local git log has the commits" local_log_has_commits "$want"
step 8 "land --worktree <rd-x> --release-sandbox" land_release
step 9a "ledger record gone" record_gone
step 9b "sandbox absent from the org listing" listing_lacks_name
step 10 "env reap --provider $provider is clean" reap_clean
snapshot_records
step 11 "wt new rd-y --env $env_name" in_repo "$THEGN_BIN" wt new rd-y --env "$env_name"
step 11p "resolve the rd-y worktree path" resolve_rdy_path
step 11u "env up (provision rd-y)" no_stdin "$THEGN_BIN" env up --worktree "$RDY_PATH"
step 11b "exactly one new ledger record for rd-y" one_new_record
echo "sandbox (rd-y): $RDY_NAME"
if [ -n "$EXEC_BRIDGE" ]; then
  step 11m "commit inside rd-y" rdy_commit
  step 11k "land rd-y without --release-sandbox keeps the sandbox" rdy_land_keep
  step 11j "rd-y's record is still ready and the listing has it" rdy_ready_and_listed
  step 11o "leave an uncommitted file inside rd-y" rdy_dirty_file
  step 11g "land --release-sandbox keeps a dirty sandbox" rdy_land_dirty_release
  step 11e "env down refuses a dirty sandbox" rdy_down_dirty
  step 11f "remove the uncommitted file" rdy_clean_dirty_file
  step 11n "commit inside rd-y again, left unpulled" rdy_commit_again
  step 11d "env deprovision of rd-y's sandbox refuses" rdy_deprovision_bound
fi
# --force only here: rd-y is this run's scratch worktree.
step 11r "wt rm <rd-y path> --force exits 0 and pulls tg/rd-y" rdy_rm_force
if [ -n "$EXEC_BRIDGE" ]; then
  step 11q "local tg/rd-y has the second sandbox commit" rdy_second_commit_landed
fi
step 11c "rd-y's ledger record is gone and the org listing lacks it" rdy_gone
if [ "$with_pool" = 1 ]; then
  step P0 "wt new rd-p --env $env_name" in_repo "$THEGN_BIN" wt new rd-p --env "$env_name"
  step P0p "resolve the rd-p worktree path" resolve_pool_path
  step P1 "fill one warm pool spare" pool_fill_one
  step P2 "exactly one new ledger record is the pool spare" pool_new_record
  snapshot_records
  step P3 "P3 claims the pool spare without a new ledger record" pool_claim_run
  step P4 "pool-fill target 0 exits 0" in_repo "$THEGN_BIN" env pool-fill --worktree "$POOL_PATH" --size 0 --json
  step P5 "wt rm rd-p destroys the spare and removes its record" pool_remove
fi
if [ -n "$adopt_name" ]; then
  step A "env adopt $adopt_name" adopt_step
fi
if [ "$with_reaper" = 1 ]; then
  echo "NOTE R: lifetime_reap needs a client up and a throwaway env with max_lifetime_secs > 0;" >&2
  echo "        verify manually with: thegn fleet list after the 300 s hydrate cadence" >&2
fi
live_finish
