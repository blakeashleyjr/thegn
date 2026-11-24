#!/usr/bin/env bash
# Live Vercel Sandbox run (THE-2234). SPENDS REAL MONEY on the owner's Vercel
# account when acting; with no flags it only prints the plan and exits 0.
#
# Neither this nor test/sprites-live.sh is in ci/ci-local/smoke.
# NOT part of `just ci`, `ci-local` or `smoke`: nothing in justfile, the
# workflows or the nix checks globs `test/*live*.sh`; it runs only through
# `just vercel-live`. The token is never read or printed: only the SecretRef
# string is written into a throwaway config.
#
# Usage:
#   test/vercel-live.sh                      # print plan, no network
#   THEGN_CHANNEL=dev test/vercel-live.sh --token-ref keyring:vercel-token \
#     --team-id team_x --project-id prj_x --i-accept-spend [--with-model-login] \
#     [--login-home-claude <dir>] [--with-headless] [--keep-going]
set -euo pipefail

THEGN_BIN="${THEGN_BIN:-thegn}"
ENV_NAME="vxlive"
MAX_LIFETIME=3600
MAX_INSTANCES=1
VCPUS=2

token_ref="" team_id="" project_id="" accept=0 model_login=0 headless=0 keep_going=0 login_home_claude=""
while [ $# -gt 0 ]; do
  case "$1" in
  --token-ref)
    token_ref="${2:?--token-ref needs a value}"
    shift 2
    ;;
  --team-id)
    team_id="${2:?--team-id needs a value}"
    shift 2
    ;;
  --project-id)
    project_id="${2:?--project-id needs a value}"
    shift 2
    ;;
  --i-accept-spend)
    accept=1
    shift
    ;;
  --with-model-login)
    model_login=1
    shift
    ;;
  --with-headless)
    headless=1
    shift
    ;;
  --keep-going)
    keep_going=1
    shift
    ;;
  --login-home-claude)
    login_home_claude="${2:?--login-home-claude needs a dir}"
    [ -d "$login_home_claude" ] || {
      echo "--login-home-claude: not a directory" >&2
      exit 2
    }
    case "$login_home_claude" in *[\"\\]* | *$'\n'*)
      echo "--login-home-claude: path must not contain a quote, backslash or newline" >&2
      exit 2
      ;;
    esac
    model_login=1
    headless=1
    shift 2
    ;;
  -h | --help)
    sed -n 2,18p "$0"
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
# --with-headless provisions a second sandbox (step 7w); the cap must allow it.
if [ "$headless" = 1 ]; then MAX_INSTANCES=2; fi

print_plan() {
  cat <<PLAN
vercel-live plan (nothing is created without all of: --token-ref, --team-id,
--project-id, --i-accept-spend and THEGN_CHANNEL=dev)
--keep-going alone is plan-only and does not bypass --i-accept-spend.

Will create : ONE Vercel sandbox, ${VCPUS} vCPU, max_lifetime_secs = ${MAX_LIFETIME},
              max_instances = ${MAX_INSTANCES}, in a fresh temp XDG state/config dir
              and a scratch git repo. The token value is never read or printed.
Max spend   : bounded by ${VCPUS} vCPU x 1 h of active CPU plus memory and snapshot
              storage; roughly under US\$1 at Pro iad1 rates (estimate).
Teardown    : trap on EXIT/INT/TERM -> env down, env reap --provider vercel
              --apply --yes (temp state), then each sandbox from this run's
              ledger (and the named sandbox) must be absent from fleet list.
              Partial or bad JSON is unknown and counts as present.
The preflight prints which credentials enter the sandbox.
Steps:
  1   create env config (temp)
  0   preflight: doctor --json remote_live for ${ENV_NAME} (after the temp
      config exists); FAIL if the token does not resolve; INFO forwarded_env,
      git_token, login_upload; WARN lists unbound sandboxes (never touched);
      SKIP on an older binary
  2   env test (token works)
  3   provision sandbox (env up)
  3b  exactly one vercel ledger record for this run
  3s  response shapes parsed (fleet list --json)
  4   run a command in the sandbox (vercel-exec)
  4b  exit status and stderr reach the caller
  5   interactive terminal open       5b  record the interactive host
  6   commit in sandbox               6b  in-sandbox curl must fail (egress)
  6b-m..6e upload mode (only --with-model-login)
  7   env pull (fast-forward)         8   stop (env down --stop), 8a resume
  8s  fleet list shows the sandbox stopped (the stop really ended compute)
  8r  fleet list shows it running again after 8a
  7s  Vercel usage response shapes parsed (fleet cost --json)
  8b  tmpfs login dir is gone after stop/resume
  7h  (--with-headless) agent run --wait --pull with model_credentials =
      "upload" (+ optional --login-home-claude <dir> -> login_homes), then
      7l land --worktree <vx-h path> --release-sandbox and 7r release
      check (only the first sandbox's ledger record remains); needs a
      second sandbox, so max_instances = 2 with --with-headless
  9   env down (destroy)              9b  no ledger record for this run's sandbox
  9c  fleet list classifies this sandbox as down, not missing (older binaries
      pass with a "binary predates batch 170" note)
 10   env reap dry-run, 10a apply
Each step prints PASS/FAIL; the first FAIL tears down and exits non-zero unless
--keep-going is set. With --keep-going, the run finishes and prints FAILED STEPS:
with the failed step ids before exiting non-zero.
Each step also prints TIME <id> <seconds> s; the end prints STARTUP lines (start-up numbers).
PLAN
}

# shellcheck source=test/lib/live.sh disable=SC1091
. "$(dirname "${BASH_SOURCE[0]}")/lib/live.sh"
# ---- flag gate: nothing above this line touches the network or thegn ----
if [ -z "$token_ref" ] && [ -z "$team_id" ] && [ -z "$project_id" ] && [ "$accept" = 0 ]; then
  print_plan
  exit 0
fi
missing=""
[ -n "$token_ref" ] || missing="$missing --token-ref"
[ -n "$team_id" ] || missing="$missing --team-id"
[ -n "$project_id" ] || missing="$missing --project-id"
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
case "$token_ref" in
keyring:* | file:* | env:*) ;;
*)
  echo "refusing: --token-ref must be a SecretRef (keyring:/file:/env:)" >&2
  exit 2
  ;;
esac

# ---- step 0: preflight (doctor's "Remote live readiness" block) ----
preflight_capture() { live_preflight_capture; }
preflight_check() { live_preflight_check "$@"; }

TMP="$(mktemp -d)"
export XDG_STATE_HOME="$TMP/state" XDG_CONFIG_HOME="$TMP/config" THEGN_DIR="$TMP/thegn"
unset THEGN_CONFIG_DIR
mkdir -p "$XDG_STATE_HOME" "$XDG_CONFIG_HOME/thegn" "$THEGN_DIR" "$TMP/repo"
mkdir -m 700 "$TMP/run"
export XDG_RUNTIME_DIR="$TMP/run"
REPO="$TMP/repo"
SANDBOX_NAME="(unknown)"
CREATED=0
STEP_9_PASSED=0
# shellcheck disable=SC2034 # appended to and read by test/lib/live.sh
FAILED_STEPS=()
# shellcheck disable=SC2034 # read by test/lib/live.sh
LIVE_STARTUP=("3=create (env up)" "4=first command (vercel-exec -- true)" "8=stop (env down --stop)" "8a=resume (env up)")

live_on_pass() { if [ "$1" = 9 ]; then STEP_9_PASSED=1; fi; }

print_plan
echo
echo "== About to create: 1 sandbox, ${VCPUS} vCPU, max_lifetime_secs=${MAX_LIFETIME}, max_instances=${MAX_INSTANCES} =="
echo "== temp state: $TMP =="

ledger_dir() { printf '%s\n' "$XDG_STATE_HOME/thegn/vps"; }
run_records() { # run_records <provider>: names of this run's ledger records
  local f
  for f in "$(ledger_dir)"/*.json; do
    [ -f "$f" ] || continue
    if grep -Eq "\"provider\": *\"$1\"" "$f"; then basename "$f" .json; fi
  done
  return 0
}

listing_lacks_name() { # listing_lacks_name <name>: partial/failed listing is unknown
  local name="$1" out
  out="$("$THEGN_BIN" fleet list --env "$ENV_NAME" --json 2>/dev/null)" || return 1
  printf '%s' "$out" | LISTED_NAME="$name" python3 -I -c '
import json, os, sys

try:
    listing = json.load(sys.stdin)
    view = listing["view"]
    rows = view["rows"]
except (AttributeError, KeyError, TypeError, ValueError):
    sys.exit(1)
if view.get("partial"):
    sys.exit(1)
if not isinstance(rows, list):
    sys.exit(1)
name = os.environ["LISTED_NAME"]
sys.exit(1 if any(isinstance(row, dict) and row.get("id") == name for row in rows) else 0)
'
}

fleet_state_is() { # fleet_state_is <state>: this run's sandbox row in a complete fleet listing has that state
  local state="$1" out
  out="$("$THEGN_BIN" fleet list --env "$ENV_NAME" --json 2>/dev/null)" || return 1
  printf '%s' "$out" | FLEET_SANDBOX="$SANDBOX_NAME" FLEET_STATE="$state" python3 -I -c '
import json, os, sys

try:
    listing = json.load(sys.stdin)
    view = listing["view"]
    rows = view["rows"]
except (AttributeError, KeyError, TypeError, ValueError):
    sys.exit(1)
if not isinstance(view, dict) or view.get("partial"):
    sys.exit(1)
if not isinstance(rows, list):
    sys.exit(1)
name = os.environ["FLEET_SANDBOX"]
state = os.environ["FLEET_STATE"]
for row in rows:
    if isinstance(row, dict) and row.get("id") == name:
        sys.exit(0 if row.get("state") == state else 1)
sys.exit(1)
'
}

wait_state() { # wait_state <tries> <state>: poll every 5 s
  local _
  for _ in $(seq 1 "$1"); do
    fleet_state_is "$2" && return 0
    sleep 5
  done
  return 1
}

teardown() {
  local rc=$?
  trap - EXIT INT TERM
  set +e
  local left=0
  if [ "$CREATED" = 1 ]; then
    local names n
    names="$(run_records vercel)"
    if [ "$STEP_9_PASSED" = 0 ]; then
      echo "-- teardown: env down"
      (cd "$REPO" && "$THEGN_BIN" env down --force)
    fi
    echo "-- teardown: env reap"
    "$THEGN_BIN" env reap --provider vercel --apply --yes
    echo "-- teardown: fleet list (run-owned names must be absent)"
    while IFS= read -r n; do
      [ -n "$n" ] || continue
      if [ -f "$(ledger_dir)/$n.json" ] || ! listing_lacks_name "$n"; then
        left=1
        rc=1
        echo "TEARDOWN INCOMPLETE: sandbox $n may still exist; the ledger is kept in $TMP" >&2
      fi
    done <<<"$names"
    if [ -n "$SANDBOX_NAME" ] && {
      [ "$SANDBOX_NAME" = "(unknown)" ] ||
        [ -f "$(ledger_dir)/$SANDBOX_NAME.json" ] ||
        ! listing_lacks_name "$SANDBOX_NAME"
    }; then
      left=1
      rc=1
      echo "TEARDOWN INCOMPLETE: sandbox $SANDBOX_NAME may still exist; the ledger is kept in $TMP" >&2
    fi
  fi
  if [ "$left" = 1 ]; then
    echo "-- keeping $TMP (ownership ledger needed for a later reap); ledger records:" >&2
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

ask() { # ask <prompt>: operator confirmation, y (or n/a) passes
  local a
  read -r -p "$1 [y/N] " a
  [ "$a" = y ] || [ "$a" = n/a ]
}

in_repo() { (cd "$REPO" && "$@"); }
no_stdin() { "$@" </dev/null; }
fails() { ! "$@"; }

name_from_ledger() { # exactly one record, safe charset
  local n
  n="$(run_records vercel)"
  [ -n "$n" ] && [ "$(printf '%s\n' "$n" | wc -l)" -eq 1 ] || return 1
  case "$n" in *[!a-z0-9-]*) return 1 ;; esac
  SANDBOX_NAME="$n"
}

egress_denied() {
  local out
  out="$("$THEGN_BIN" vercel-exec "$SANDBOX_NAME" -- sh -c 'curl -m 5 -sS -o /dev/null https://example.com; echo "curl_rc=$?"' </dev/null)" || return 1
  printf '%s\n' "$out" | grep 'curl_rc=' >&2 || echo "no curl_rc line in output: $out" >&2
  case "$out" in
  *curl_rc=0*)
    echo "egress is OPEN: curl reached example.com (curl_rc=0)" >&2
    return 1
    ;;
  *curl_rc=127*)
    echo "curl missing in the sandbox image; egress not tested (curl_rc=127)" >&2
    return 1
    ;;
  *curl_rc=*) return 0 ;;
  *) return 1 ;;
  esac
}

response_shapes_parsed() {
  local out
  out="$("$THEGN_BIN" fleet list --env "$ENV_NAME" --json)" || return 1
  FLEET_JSON="$out" FLEET_SANDBOX="$SANDBOX_NAME" python3 -I -c '
import json, os, sys

try:
    listing = json.loads(os.environ["FLEET_JSON"])
    view = listing["view"]
    rows = view["rows"]
except (KeyError, TypeError, ValueError):
    print("status not parsed: the Vercel response shape differs from thegn'"'"'s parser", file=sys.stderr)
    print("createdAt not parsed: the lifetime ceiling cannot work", file=sys.stderr)
    sys.exit(1)

if not isinstance(rows, list):
    print("status not parsed: the Vercel response shape differs from thegn'"'"'s parser", file=sys.stderr)
    print("createdAt not parsed: the lifetime ceiling cannot work", file=sys.stderr)
    sys.exit(1)

name = os.environ["FLEET_SANDBOX"]
row = next((r for r in rows if isinstance(r, dict) and r.get("id") == name), None)
failed = False
if row is None or row.get("state") not in {"running", "pending"}:
    print("status not parsed: the Vercel response shape differs from thegn'"'"'s parser", file=sys.stderr)
    failed = True

created_ms = row.get("created_ms") if row is not None else None
if isinstance(created_ms, bool) or not isinstance(created_ms, (int, float)) or created_ms <= 0:
    print("createdAt not parsed: the lifetime ceiling cannot work", file=sys.stderr)
    failed = True

if row is not None and row.get("size") is None:
    print("WARN 3s: size not parsed")
if failed:
    sys.exit(1)
print("   Vercel response shape: " + json.dumps(row, sort_keys=True))
'
}

exit_status_and_stderr_reach_caller() {
  local out rc
  if out="$(no_stdin "$THEGN_BIN" vercel-exec "$SANDBOX_NAME" -- sh -c 'echo boom >&2; exit 3' 2>&1)"; then
    rc=0
  else
    rc=$?
  fi
  printf '%s\n' "$out"
  echo "   vercel-exec exit status: $rc"
  [ "$rc" -eq 3 ] && [[ $out == *boom* ]]
}

usage_shapes_parsed() {
  local out
  out="$("$THEGN_BIN" fleet cost --json)" || return 1
  COST_JSON="$out" COST_SANDBOX="$SANDBOX_NAME" python3 -I -c '
import json, os, sys

try:
    report = json.loads(os.environ["COST_JSON"])
except ValueError:
    print("Vercel usage not parsed: fleet cost JSON is invalid", file=sys.stderr)
    sys.exit(1)

usage = report.get("vercel_usage") if isinstance(report, dict) else None
if not isinstance(usage, list):
    print("Vercel usage not parsed: fleet cost JSON has no vercel_usage", file=sys.stderr)
    sys.exit(1)

name = os.environ["COST_SANDBOX"]
row = None
for block in usage:
    if not isinstance(block, dict) or not isinstance(block.get("rows"), list):
        continue
    row = next((r for r in block["rows"] if isinstance(r, dict) and r.get("name") == name), None)
    if row is not None:
        break
if row is None:
    print("Vercel usage not parsed: no row for the sandbox", file=sys.stderr)
    sys.exit(1)

status = row.get("status")
if status is None or status == "unknown":
    print("status not parsed: the Vercel response shape differs from thegn'"'"'s parser", file=sys.stderr)
    sys.exit(1)
if row.get("session_usd") is None:
    print("WARN 7s: usage fields not parsed")
print("   Vercel usage row: " + json.dumps({
    key: row.get(key) for key in (
        "name", "status", "session_usd", "snapshots_usd_month", "snapshot_bytes"
    )
}, sort_keys=True))
'
}

VX_PATH=""
resolve_vx_path() { # the worktree whose branch ends in vx-h
  VX_PATH="$(git -C "$REPO" worktree list --porcelain | awk '/^worktree /{p=substr($0,10)} /^branch .*vx-h$/{print p}')"
  echo "   worktree: ${VX_PATH:-(none)}"
  [ -n "$VX_PATH" ] && [ "$(printf '%s\n' "$VX_PATH" | wc -l)" -eq 1 ]
}
sandbox_record_gone() { [ ! -f "$(ledger_dir)/$SANDBOX_NAME.json" ]; }
released_headless() { [ "$(run_records vercel | wc -l)" -eq 1 ]; }

write_config() {
  cat >"$XDG_CONFIG_HOME/thegn/config.toml" <<CFG
[env.${ENV_NAME}]
placement = "provider"

[env.${ENV_NAME}.provider]
provider = "vercel"
team_id = "${team_id}"
project_id = "${project_id}"
api_key_env = "${token_ref}"
size = "${VCPUS}"
max_instances = ${MAX_INSTANCES}
max_lifetime_secs = ${MAX_LIFETIME}
CFG
  if [ "$model_login" = 1 ]; then
    # Explicit user-config opt-in; the wizard/`env create` never write this.
    echo 'model_credentials = "upload"' >>"$XDG_CONFIG_HOME/thegn/config.toml"
    if [ -n "$login_home_claude" ]; then
      printf 'login_homes = { claude = "%s" }\n' "$login_home_claude" >>"$XDG_CONFIG_HOME/thegn/config.toml"
    fi
  fi
}

scratch_repo() {
  git -C "$REPO" init -q -b main &&
    git -C "$REPO" -c user.name=vx -c user.email=vx@example.invalid -c commit.gpgsign=false \
      commit -q --allow-empty -m init &&
    git -C "$REPO" switch -q -c vx/live-lane
}

setup_run() { write_config && scratch_repo; }

step 1 "write temp config + scratch repo" setup_run
# The env exists only in the temp config written above, so doctor runs here.
preflight_capture
preflight_check "$ENV_NAME"
step 2 "env test (token resolves, list() works)" "$THEGN_BIN" env test "$ENV_NAME"
in_repo "$THEGN_BIN" env set "$ENV_NAME" >/dev/null
CREATED=1
step 3 "provision sandbox (env up)" in_repo "$THEGN_BIN" env up
step 3b "exactly one vercel ledger record for this run" name_from_ledger
echo "sandbox: $SANDBOX_NAME"
step 3s "response shapes parsed" response_shapes_parsed
step 4 "command runs in the sandbox (vercel-exec)" no_stdin "$THEGN_BIN" vercel-exec "$SANDBOX_NAME" -- true
step 4b "exit status and stderr reach the caller" exit_status_and_stderr_reach_caller
echo "   run in ANOTHER terminal: XDG_STATE_HOME='$XDG_STATE_HOME' XDG_CONFIG_HOME='$XDG_CONFIG_HOME' XDG_RUNTIME_DIR='$XDG_RUNTIME_DIR' THEGN_CHANNEL=dev $THEGN_BIN  (open $REPO)"
echo "== step 5: open an interactive terminal pane for this worktree now (manual)"
echo "   (a pane open runs the vercel-exec TTY bridge; then close it)"
step 5 "interactive terminal opened (operator confirms)" ask "pane opened and closed cleanly? "
step 5b "bridge host line (stderr of the pane bridge: admitted ticket host, /health host)" ask "host line read and recorded (host only, no URL query/ticket)? "
step 6b "in-sandbox egress is denied (curl to example.com must fail)" egress_denied
step 6 "commit inside the sandbox (operator, in the pane)" ask "committed inside the sandbox? "
if [ "$model_login" = 1 ]; then
  step 6b-m "egress policy narrowed to model hosts before upload" ask "doctor shows narrowed egress? "
  step 6c "login dir exists on /dev/shm, dir 0700 files 0600" ask "stat shows tmpfs + modes ok? "
  step 6d "claude starts with only .credentials.json + synthesized .claude.json" ask "claude ran a prompt? "
  step 6e "model hosts list is complete (no blocked host in logs)" ask "no egress denials for model hosts? "
else
  echo "SKIP 6b-6e (upload mode needs --with-model-login and a host claude login)"
fi
step 7 "env pull fast-forwards the lane branch" in_repo "$THEGN_BIN" env pull
step 7s "Vercel usage response shapes parsed" usage_shapes_parsed
if [ "$headless" = 1 ]; then
  step 7w "wt new vx-h" in_repo "$THEGN_BIN" wt new vx-h --env "$ENV_NAME"
  step 7p "resolve the vx-h worktree path" resolve_vx_path
  step 7h "headless agent run --wait --pull (upload credentials)" in_repo "$THEGN_BIN" agent run --worktree "$VX_PATH" --agent claude --prompt "create vx.txt containing ok and commit it" --wait --pull --json
  step 7l "land the headless worktree" in_repo "$THEGN_BIN" land --worktree "$VX_PATH" --release-sandbox
  step 7r "release check: land released the headless sandbox" released_headless
fi
step 8 "stop (env down --stop)" in_repo "$THEGN_BIN" env down --stop
step 8s "stop reached Vercel (fleet list shows the sandbox stopped)" wait_state 12 stopped
step 8a "resume (env up)" in_repo "$THEGN_BIN" env up
step 8r "resume reached Vercel (fleet list shows it running)" wait_state 12 running
step 8b "tmpfs login dir is gone after stop/resume" ask "/dev/shm/thegn-login-* absent after resume? "
step 9 "env down destroys the sandbox" in_repo "$THEGN_BIN" env down
step 9b "no ledger record for this run's sandbox" sandbox_record_gone
fleet_down_not_missing() {
  local out
  [ ! -f "$(ledger_dir)/$SANDBOX_NAME.json" ] || {
    echo "   ledger record for $SANDBOX_NAME is still present" >&2
    return 1
  }
  out="$("$THEGN_BIN" fleet list --env "$ENV_NAME" --json)" || return 1
  printf '%s' "$out" | FLEET_SANDBOX="$SANDBOX_NAME" python3 -I -c '
import json, os, sys

try:
    listing = json.load(sys.stdin)
    view = listing["view"]
except (AttributeError, KeyError, TypeError, ValueError):
    print("fleet list JSON is invalid or missing view", file=sys.stderr)
    sys.exit(1)

if not isinstance(view, dict):
    print("fleet list JSON view is not an object", file=sys.stderr)
    sys.exit(1)
if "down" not in view:
    print("fleet list JSON has no down key: binary predates batch 170")
    sys.exit(0)
try:
    down = view["down"]
    missing = view["missing"]
except KeyError as exc:
    print("fleet list JSON is missing key " + str(exc.args[0]), file=sys.stderr)
    sys.exit(1)
if not isinstance(down, list) or not isinstance(missing, list):
    print("fleet list JSON view.down and view.missing must be arrays", file=sys.stderr)
    sys.exit(1)
sandbox = os.environ["FLEET_SANDBOX"]
in_down = any(isinstance(row, dict) and row.get("id") == sandbox for row in down)
in_missing = any(isinstance(row, dict) and row.get("id") == sandbox for row in missing)
if not in_down or in_missing:
    print("fleet list JSON did not classify the sandbox as down (and not missing)", file=sys.stderr)
    sys.exit(1)
print("   fleet truth: " + sandbox + " is down and not missing")
'
}
step 9c "fleet list classifies this sandbox as down, not missing" fleet_down_not_missing
step 10 "reap: dry-run" "$THEGN_BIN" env reap --provider vercel
step 10a "reap: apply finds nothing left" "$THEGN_BIN" env reap --provider vercel --apply --yes
live_finish
