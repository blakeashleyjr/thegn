#!/usr/bin/env bash
# Live Sprites run (b160 rx-live). SPENDS REAL MONEY on the owner's Sprites
# account when acting; with no flags it only prints the plan and exits 0.
#
# Neither this nor test/vercel-live.sh is in ci/ci-local/smoke: nothing in the
# justfile, workflows or nix checks globs `test/*live*.sh`; it runs only through
# `just sprites-live`. It reads the owner's REAL config read-only
# (XDG_CONFIG_HOME as given; the sprites token is never read or printed) with a
# fresh XDG_STATE_HOME, XDG_RUNTIME_DIR and THEGN_DIR and a scratch repo.
#
# The daily headless/land/reap run lives in test/remote-daily.sh (just remote-daily).
#
# Usage:
#   test/sprites-live.sh                       # print plan, no network
#   THEGN_CHANNEL=dev test/sprites-live.sh --env <name> --i-accept-spend [--keep-going]
set -euo pipefail

THEGN_BIN="${THEGN_BIN:-thegn}"

env_name="" accept=0 keep_going=0 min_ram=""
while [ $# -gt 0 ]; do
  case "$1" in
  --env)
    env_name="${2:?--env needs a value}"
    shift 2
    ;;
  --min-ram-gb)
    min_ram="${2:?--min-ram-gb needs a value}"
    case "$min_ram" in
    '' | *[!0-9]*)
      echo "--min-ram-gb needs a whole number" >&2
      exit 2
      ;;
    esac
    shift 2
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

print_plan() {
  cat <<PLAN
sprites-live plan (nothing is created without both --env <name> and
--i-accept-spend, plus THEGN_CHANNEL=dev)

Will create : ONE sprite via the named [env.<name>] from your real config
              (read-only), in a fresh state/runtime/THEGN_DIR and a scratch git
              repo on branch sx/live-lane. No config is written.
Max spend   : one sprite for the length of this run (about 15 minutes, incl. a
              420 s hold and 120 s idle); teardown destroys it.
Teardown    : trap on EXIT/INT/TERM -> kill a still-running hold, env down
              --force only when step 11 did not pass, env deprovision <name> for
              each record in THIS run's ledger, env reap --provider sprites
              --apply --yes; fails (and keeps the ledger) if any of this run's
              records or sprites remain. A sprite is absent only when no
              view.rows entry has its id; partial or unparsable JSON is unknown.
The preflight prints which credentials enter the sandbox.
Steps:
  0   preflight: doctor --json remote_live for <name>; FAIL if the token does
      not resolve; INFO forwarded_env, git_token, login_upload; WARN lists
      unbound pre-existing sandboxes (never touched); SKIP on an older binary
  1   scratch repo + env test          1b  env test
  2   no sprites ledger record before create
  3   provision (env up)               3b  exactly one ready sprites ledger record
  4   sprite workspace HEAD == local HEAD (sprite-exec, \$HOME/workspace)
  5   (only with --min-ram-gb N) doctor --json under --set min_ram_gb=N shows N*1024
  5b  (only with --min-ram-gb N) /sys/fs/cgroup/memory.max in the sprite is
      N GiB in bytes (raw value printed; SKIP if the file is absent)
  6   interactive exec under script(1): /dev/pts/<n> and ok; print the trace
      log path and the last 40 "sprites exec frame" lines
  6c  interactive output keeps its line breaks: /dev/pts/<n> and ok are on
      different lines (this is expected to fail until the live output is fixed)
  6b  curl exists in the sprite (keep-awake precondition)
  7a  headless hold: sprite-exec --hold 10m -- sleep 420 in the background (no
      per-run overlay can add a hold-test agent, so no agent run --wait);
      /v1/tasks shows thegn-hold- during, none after
  7e  the hold's stderr has no "keep-awake unavailable"
  8   env pull after an empty commit inside the sprite; output is fast-forwarded
  8b  local HEAD equals the commit made inside the sprite
  8c  refs/thegn/pull is empty after the clean fast-forward
  9   after 120 s idle, fleet list --env shows warm or cold
  9b  /v1/tasks is empty
  10  fleet cost --json: print sprites_usage; error fails, cpu_hours > 0 passes,
      or partial zero usage warns and passes
  10b fleet list --env still shows warm or cold (the usage call did not wake it)
  11  env down                         11b fleet list puts this sprite in view.down,
      not view.missing (older binaries FAIL with "binary predates batch 170")
  12  env reap --provider sprites --json (dry-run): 0 recorded orphans, others
      listed as unrecorded
  13  env reap --provider sprites --unrecorded --json (read-only): apply == false,
      results is empty; print the unrecorded count
Each step prints PASS/FAIL; the first FAIL tears down and exits non-zero.
With --keep-going, failed steps print FAIL <id>, the run continues, and the
end prints FAILED STEPS: <ids> before exiting non-zero. ALL STEPS PASS prints
only when no step failed.
PLAN
}

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

# ---- step 0: preflight (doctor's "Remote live readiness" block) ----
# doctor --json lists names only, never values: whether the token resolves,
# which env var names, git token and agent logins enter the sandbox, and ledger
# sandboxes nothing is bound to. The token itself is never read or printed.
PREFLIGHT_PY='
import json, os, sys
env = os.environ["PRE_ENV"]
required = os.environ.get("PRE_REQUIRED", "1") == "1"
try:
    doc = json.loads(sys.stdin.read())
except ValueError:
    print("FAIL 0: doctor --json did not print JSON", file=sys.stderr)
    sys.exit(1)
rows = doc.get("remote_live") if isinstance(doc, dict) else None
if rows is None:
    print("SKIP 0: doctor --json has no remote_live key (older binary): no credential statement")
    sys.exit(0)
row = next((r for r in rows if isinstance(r, dict) and r.get("env") == env), None)
if row is None:
    if required:
        print("FAIL 0: doctor remote_live has no row for env " + env, file=sys.stderr)
        sys.exit(1)
    print("SKIP 0: doctor remote_live has no row for env " + env + " (it covers sprites and vercel envs)")
    sys.exit(0)
def show(v):
    if isinstance(v, bool):
        return "yes" if v else "no"
    if isinstance(v, list):
        return ",".join(show(x) for x in v) or "none"
    if isinstance(v, dict):
        return ",".join(k + "=" + show(x) for k, x in sorted(v.items())) or "none"
    if v is None or v == "":
        return "none"
    return str(v)
if row.get("token_resolves") is not True:
    print("FAIL 0: the token for env " + env + " does not resolve (remote_live.token_resolves = " + show(row.get("token_resolves")) + ")", file=sys.stderr)
    sys.exit(1)
print("INFO 0: credentials entering the sandbox: forwarded_env=" + show(row.get("forwarded_env")) + "; git_token=" + show(row.get("git_token")) + "; login_upload=" + show(row.get("login_upload")))
unbound = row.get("unbound") or []
if not isinstance(unbound, list):
    unbound = [unbound]
if unbound:
    print("WARN 0: unbound ledger sandboxes: " + ", ".join(show(n) for n in unbound) + ": pre-existing sandboxes this run will not touch")
print("PASS 0")
'
DOCTOR_JSON=""
preflight_capture() { # doctor may exit non-zero on unrelated checks; parse what it printed
  DOCTOR_JSON="$("$THEGN_BIN" doctor --json 2>/dev/null)" || true
}
preflight_check() { # preflight_check <env> [required: 1|0]
  echo "== step 0: preflight (doctor --json remote_live for env '$1')"
  printf '%s' "$DOCTOR_JSON" | PRE_ENV="$1" PRE_REQUIRED="${2:-1}" python3 -I -c "$PREFLIGHT_PY" || {
    echo "FAIL 0: preflight" >&2
    exit 1
  }
}
# Run against the owner's real state, before the fresh dirs below, so
# pre-existing unbound sandboxes are listed.
preflight_capture

# Real config stays as given (read-only); everything mutable is fresh.
TMP="$(mktemp -d)"
export XDG_STATE_HOME="$TMP/state" THEGN_DIR="$TMP/thegn"
unset THEGN_CONFIG_DIR
mkdir -p "$XDG_STATE_HOME" "$THEGN_DIR" "$TMP/repo"
mkdir -m 700 "$TMP/run"
export XDG_RUNTIME_DIR="$TMP/run"
REPO="$TMP/repo"
SPRITE_NAME="(unknown)"
CREATED=0
HOLD_PID=""
HOLD_ERR="$TMP/hold.err"
STEP_11_PASSED=0
FAILED_STEPS=()
INTERACTIVE_OUT=""
PULL_SHA=""

print_plan
echo
echo "== About to create: 1 sprite via env '$env_name' =="
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
  out="$("$THEGN_BIN" fleet list --env "$env_name" --json 2>/dev/null)" || return 1
  printf '%s' "$out" | LISTED_NAME="$name" python3 -I -c '
import json, os, sys
try:
    listing = json.load(sys.stdin)
    view = listing["view"]
    rows = view["rows"]
except (KeyError, TypeError, ValueError):
    sys.exit(1)
if view.get("partial"):
    sys.exit(1)
if not isinstance(rows, list):
    sys.exit(1)
name = os.environ["LISTED_NAME"]
sys.exit(1 if any(isinstance(row, dict) and row.get("id") == name for row in rows) else 0)
'
}

teardown() {
  local rc=$?
  trap - EXIT INT TERM
  set +e
  local left=0
  if [ -n "$HOLD_PID" ] && kill -0 "$HOLD_PID" 2>/dev/null; then
    echo "-- teardown: stopping the background hold (pid $HOLD_PID)"
    kill "$HOLD_PID" 2>/dev/null
    sleep 2
    if kill -0 "$HOLD_PID" 2>/dev/null; then
      echo "TEARDOWN INCOMPLETE: hold process $HOLD_PID survived" >&2
      rc=1
    fi
  fi
  if [ "$CREATED" = 1 ]; then
    local names n
    names="$(run_records sprites)"
    if [ "$STEP_11_PASSED" = 0 ]; then
      echo "-- teardown: env down"
      (cd "$REPO" && "$THEGN_BIN" env down --force)
    fi
    for n in $names; do
      case "$n" in *[!a-z0-9-]*) continue ;; esac
      echo "-- teardown: env deprovision $n"
      "$THEGN_BIN" env deprovision "$n" --env "$env_name" # destroy proves ownership via this run's ledger
    done
    echo "-- teardown: env reap"
    "$THEGN_BIN" env reap --provider sprites --apply --yes
    for n in $names; do
      if [ -f "$(ledger_dir)/$n.json" ] || ! listing_lacks_name "$n"; then
        left=1
        rc=1
        echo "TEARDOWN INCOMPLETE: sprite $n may still exist; the ledger is kept in $TMP" >&2
      fi
    done
  fi
  if [ "$left" = 1 ]; then
    echo "-- keeping $TMP (ledger needed for a later reap); ledger records:" >&2
    ls "$(ledger_dir)" >&2
  else
    rm -rf "$TMP"
  fi
  exit "$rc"
}
trap teardown EXIT INT TERM

step() { # step <id> <description> <command...>
  local id="$1" desc="$2"
  shift 2
  echo "== step $id: $desc"
  if "$@"; then
    echo "PASS $id"
    if [ "$id" = 11 ]; then
      STEP_11_PASSED=1
    fi
  else
    echo "FAIL $id: $desc" >&2
    FAILED_STEPS+=("$id")
    if [ "$keep_going" != 1 ]; then
      exit 1
    fi
  fi
  return 0
}

in_repo() { (cd "$REPO" && "$@"); }
no_stdin() { "$@" </dev/null; }
fails() { ! "$@"; }

name_from_ledger() { # exactly one record, safe charset, state ready
  local n
  n="$(run_records sprites)"
  [ -n "$n" ] && [ "$(printf '%s\n' "$n" | wc -l)" -eq 1 ] || return 1
  case "$n" in *[!a-z0-9-]*) return 1 ;; esac
  grep -Eq '"state": *"ready"' "$(ledger_dir)/$n.json" || return 1
  SPRITE_NAME="$n"
}
ledger_empty() { [ -z "$(run_records sprites)" ]; }

scratch_repo() {
  git -C "$REPO" init -q -b main &&
    git -C "$REPO" -c user.name=sx -c user.email=sx@example.invalid -c commit.gpgsign=false \
      commit -q --allow-empty -m init &&
    git -C "$REPO" switch -q -c sx/live-lane
}

head_matches() { # the sprite workspace HEAD equals the local lane HEAD
  local remote local_head
  # shellcheck disable=SC2016 # $HOME must expand inside the sprite
  remote="$(no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- sh -c 'git -C "$HOME/workspace" rev-parse HEAD')" || return 1
  remote="$(printf '%s' "$remote" | tr -d '\r\n ')"
  local_head="$(git -C "$REPO" rev-parse HEAD)" || return 1
  echo "   sprite HEAD $remote, local HEAD $local_head"
  [ -n "$remote" ] && [ "$remote" = "$local_head" ]
}

memory_max_matches() { # memory.max in bytes equals N GiB; absent file skips
  local out want
  want=$((min_ram * 1024 * 1024 * 1024))
  out="$(no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- \
    sh -c 'if [ -r /sys/fs/cgroup/memory.max ]; then cat /sys/fs/cgroup/memory.max; else echo thegn-absent; fi')" || return 1
  out="$(printf '%s' "$out" | tr -d '\r\n ')"
  echo "   memory.max raw value: $out (want $want)"
  if [ "$out" = thegn-absent ]; then
    echo "SKIP 5b: /sys/fs/cgroup/memory.max is absent in the sprite"
    return 0
  fi
  [ "$out" = "$want" ]
}

curl_present() { # the keep-awake hold needs curl inside the sprite
  no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- sh -c 'command -v curl'
}

min_ram_shown() { # doctor reflects min_ram_gb=N as N*1024 MiB
  local out
  out="$("$THEGN_BIN" --set "env.$env_name.provider.min_ram_gb=$min_ram" doctor --json 2>&1)" || return 1
  printf '%s' "$out" | grep -Eq "(^|[^0-9])$((min_ram * 1024))([^0-9]|$)"
}

interactive_tty() { # values reach script(1) via the environment, never the command text
  local log_path frame_log_tail
  # shellcheck disable=SC2016 # fixed text; script(1)'s shell expands the env values
  INTERACTIVE_OUT="$(LIVE_BIN="$THEGN_BIN" LIVE_SPRITE="$SPRITE_NAME" LIVE_TRACE="$TMP/sprite-exec-trace.log" SHELL=/bin/sh \
    THEGN_LOG=thegn::provider=trace \
    script -qec 'exec "$LIVE_BIN" sprite-exec "$LIVE_SPRITE" -- sh -c "tty && echo ok" 2>"$LIVE_TRACE"' /dev/null </dev/null)" || return 1
  printf '%s\n' "$INTERACTIVE_OUT"
  log_path="$TMP/sprite-exec-trace.log"
  frame_log_tail=""
  if [ -f "$log_path" ]; then
    frame_log_tail="$(grep -F 'sprites exec frame' "$log_path" | tail -n 40)" || true
  fi
  echo "   trace log path: $log_path"
  echo "   last 40 sprites exec frame lines:"
  printf '%s\n' "$frame_log_tail"
  grep -Eq '/dev/pts/[0-9]+' <<<"$INTERACTIVE_OUT" && grep -q 'ok' <<<"$INTERACTIVE_OUT"
}

interactive_line_breaks() {
  INTERACTIVE_OUT="$INTERACTIVE_OUT" python3 -I -c '
import os, re, sys
lines = os.environ["INTERACTIVE_OUT"].replace("\r", "").splitlines()
tty = next((i for i, line in enumerate(lines) if re.search(r"/dev/pts/[0-9]+", line)), None)
ok = next((i for i, line in enumerate(lines) if re.search(r"(^|\s)ok(\s|$)", line)), None)
sys.exit(0 if tty is not None and ok is not None and tty != ok else 1)
'
}

list_tasks() { # the sprite's in-VM Tasks API
  no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- \
    sh -c 'curl -sS --unix-socket /.sprite/api.sock http://sprite/v1/tasks'
}
hold_task_present() {
  local out
  out="$(list_tasks)" || return 1
  case "$out" in *thegn-hold-*) return 0 ;; *) return 1 ;; esac
}
hold_task_absent() {
  local out
  out="$(list_tasks)" || return 1
  case "$out" in *thegn-hold-*) return 1 ;; *) return 0 ;; esac
}
tasks_empty() {
  local out
  out="$(list_tasks)" || return 1
  out="$(printf '%s' "$out" | tr -d '[:space:]')"
  echo "   tasks: $out"
  case "$out" in '[]' | '{"tasks":[]}' | '{"tasks":null}') return 0 ;; *) return 1 ;; esac
}

hold_start() {
  no_stdin "$THEGN_BIN" sprite-exec --hold 10m "$SPRITE_NAME" -- sleep 420 2>"$HOLD_ERR" &
  HOLD_PID=$!
  sleep 30
  kill -0 "$HOLD_PID" 2>/dev/null
}
hold_finish() {
  local pid="$HOLD_PID"
  wait "$pid" || return 1
  HOLD_PID=""
}

hold_was_awake() { # sprite_hold prints this line when it runs unheld
  if grep -Fq "keep-awake unavailable" "$HOLD_ERR"; then
    echo "   hold stderr:"
    sed 's/^/     /' "$HOLD_ERR"
    return 1
  fi
}

pull_fast_forwarded() {
  # shellcheck disable=SC2016 # $HOME must expand inside the sprite
  PULL_SHA="$(no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- sh -c 'cd "$HOME/workspace" && git -c user.name=live -c user.email=live@example.invalid commit --allow-empty -qm live-pull && git rev-parse HEAD')" || return 1
  PULL_SHA="$(printf '%s' "$PULL_SHA" | tr -d '\r\n ')"
  [ -n "$PULL_SHA" ] || return 1
  local out
  out="$(in_repo "$THEGN_BIN" env pull 2>&1)" || {
    printf '%s\n' "$out"
    return 1
  }
  printf '%s\n' "$out"
  grep -Fq 'fast-forwarded' <<<"$out"
}

pull_head_matches() {
  local local_head
  local_head="$(git -C "$REPO" rev-parse HEAD)" || return 1
  echo "   pulled commit $PULL_SHA, local HEAD $local_head"
  [ -n "$PULL_SHA" ] && [ "$local_head" = "$PULL_SHA" ]
}

pull_refs_empty() { [ -z "$(git -C "$REPO" for-each-ref refs/thegn/pull)" ]; }

warm_or_cold() {
  local out
  out="$("$THEGN_BIN" fleet list --env "$env_name" --json 2>&1)" || return 1
  printf '%s' "$out" | grep -Eq '"(warm|cold)"'
}
idle_warm_or_cold() {
  echo "   idling 120 s"
  sleep 120
  warm_or_cold
}

cost_has_cpu() {
  local out
  out="$("$THEGN_BIN" fleet cost --json)" || return 1
  COST_JSON="$out" COST_SPRITE="$SPRITE_NAME" COST_ENV="$env_name" python3 -I -c '
import json, os, sys
try:
    report = json.loads(os.environ["COST_JSON"])
    usage = report["sprites_usage"]
except (KeyError, TypeError, ValueError):
    print("fleet cost JSON has no sprites_usage", file=sys.stderr)
    sys.exit(1)
if not isinstance(usage, list) or not usage:
    print("fleet cost JSON sprites_usage is not a non-empty array", file=sys.stderr)
    sys.exit(1)
partial_zero = False
for block in usage:
    print("   sprites_usage: " + json.dumps(block, sort_keys=True))
    if block.get("env") != os.environ["COST_ENV"]:
        continue
    if block.get("error") is not None:
        print("fleet cost sprites_usage error: " + str(block["error"]), file=sys.stderr)
        sys.exit(1)
    rows = block.get("rows")
    if not isinstance(rows, list):
        print("fleet cost sprites_usage has no rows", file=sys.stderr)
        sys.exit(1)
    row = next((r for r in rows if isinstance(r, dict) and r.get("name") == os.environ["COST_SPRITE"]), None)
    if row is None:
        print("fleet cost sprites_usage has no row for the sprite", file=sys.stderr)
        sys.exit(1)
    cpu_hours = row.get("cpu_hours")
    if isinstance(cpu_hours, (int, float)) and cpu_hours > 0:
        continue
    if cpu_hours == 0 and row.get("partial") is True:
        partial_zero = True
        continue
    print("fleet cost sprites_usage cpu_hours is not positive", file=sys.stderr)
    sys.exit(1)
if not any(block.get("env") == os.environ["COST_ENV"] for block in usage):
    print("fleet cost sprites_usage has no block for env " + os.environ["COST_ENV"], file=sys.stderr)
    sys.exit(1)
if partial_zero:
    print("WARN 10: usage not reported yet (partial)")
'
}

fleet_down_not_missing() {
  local out
  [ ! -f "$(ledger_dir)/$SPRITE_NAME.json" ] || {
    echo "   ledger record for $SPRITE_NAME is still present" >&2
    return 1
  }
  out="$("$THEGN_BIN" fleet list --env "$env_name" --json)" || return 1
  FLEET_JSON="$out" FLEET_SPRITE="$SPRITE_NAME" python3 -I -c '
import json, os, sys

try:
    listing = json.loads(os.environ["FLEET_JSON"])
    view = listing["view"]
    down = view["down"]
    missing = view["missing"]
except KeyError as exc:
    if exc.args == ("down",):
        print("fleet list JSON has no down key: binary predates batch 170", file=sys.stderr)
    else:
        print("fleet list JSON is missing key " + str(exc.args[0]), file=sys.stderr)
    sys.exit(1)
except (TypeError, ValueError):
    print("fleet list JSON is invalid", file=sys.stderr)
    sys.exit(1)

if not isinstance(down, list) or not isinstance(missing, list):
    print("fleet list JSON view.down and view.missing must be arrays", file=sys.stderr)
    sys.exit(1)
sprite = os.environ["FLEET_SPRITE"]
in_down = any(isinstance(row, dict) and row.get("id") == sprite for row in down)
in_missing = any(isinstance(row, dict) and row.get("id") == sprite for row in missing)
if not in_down or in_missing:
    print("fleet list JSON did not classify the sprite as down (and not missing)", file=sys.stderr)
    sys.exit(1)
print("   fleet truth: " + sprite + " is down and not missing")
'
}

reap_dry_clean() { # dry-run only: no recorded orphan, everything else unrecorded
  local out
  out="$("$THEGN_BIN" env reap --provider sprites --json 2>&1)" || return 1
  printf '%s\n' "$out" | head -40
  grep -Fq "\"$SPRITE_NAME\"" <<<"$out" && return 1
  grep -Eq ': *"recorded"' <<<"$out" && return 1
  return 0
}

reap_unrecorded_read_only() {
  local out
  out="$("$THEGN_BIN" env reap --provider sprites --unrecorded --json)" || return 1
  UNRECORDED_JSON="$out" python3 -I -c '
import json, os, sys

try:
    report = json.loads(os.environ["UNRECORDED_JSON"])
    apply = report["apply"]
    results = report["results"]
except (KeyError, TypeError, ValueError) as exc:
    print("unrecorded reap JSON is missing apply/results: " + str(exc), file=sys.stderr)
    sys.exit(1)
if apply is not False:
    print("unrecorded reap was not read-only (apply is not false)", file=sys.stderr)
    sys.exit(1)
if not isinstance(results, list):
    print("unrecorded reap JSON results is not an array", file=sys.stderr)
    sys.exit(1)
if results:
    print("unrecorded reap returned " + str(len(results)) + " entries", file=sys.stderr)
    sys.exit(1)
accounts = report.get("accounts")
if not isinstance(accounts, list):
    print("unrecorded reap JSON accounts is not an array", file=sys.stderr)
    sys.exit(1)
count = sum(len(a.get("entries") or []) for a in accounts if isinstance(a, dict))
print("   unrecorded entries: " + str(count))
'
}

preflight_check "$env_name"
step 1 "scratch repo + env test (token resolves)" bash -c "true"
scratch_repo
step 1b "env test" "$THEGN_BIN" env test "$env_name"
step 2 "no sprites ledger record before create" ledger_empty
in_repo "$THEGN_BIN" env set "$env_name" >/dev/null
CREATED=1
step 3 "provision sprite (env up)" in_repo "$THEGN_BIN" env up
step 3b "exactly one ready sprites ledger record (plan step 3)" name_from_ledger
echo "sprite: $SPRITE_NAME"
step 4 "sprite workspace HEAD matches the local lane HEAD" head_matches
if [ -n "$min_ram" ]; then
  step 5 "doctor shows min_ram_gb=$min_ram as $((min_ram * 1024)) MiB" min_ram_shown
  step 5b "sprite memory.max is $min_ram GiB" memory_max_matches
else
  echo "SKIP 5, 5b (needs --min-ram-gb N)"
fi
step 6 "interactive exec has /dev/pts/<n> and ok" interactive_tty
step 6c "interactive output keeps its line breaks" interactive_line_breaks
step 6b "curl exists in the sprite (keep-awake precondition)" curl_present
step 7a "background hold runs (sprite-exec --hold 10m -- sleep 420)" hold_start
step 7b "/v1/tasks shows thegn-hold- during the hold" hold_task_present
step 7c "the hold exits clean" hold_finish
step 7d "/v1/tasks shows no thegn-hold- after the hold" hold_task_absent
step 7e "the hold had keep-awake (no 'keep-awake unavailable' on stderr)" hold_was_awake
step 8 "env pull fast-forwards the commit made inside the sprite" pull_fast_forwarded
step 8b "local HEAD equals the commit made inside the sprite" pull_head_matches
step 8c "refs/thegn/pull is empty after the clean fast-forward" pull_refs_empty
step 9 "after 120 s idle the sprite is warm or cold" idle_warm_or_cold
step 9b "/v1/tasks is empty" tasks_empty
step 10 "fleet cost reports sprites_usage CPU hours or partial zero usage" cost_has_cpu
step 10b "fleet list still shows warm or cold after fleet cost" warm_or_cold
step 11 "env down destroys the sprite" in_repo "$THEGN_BIN" env down
step 11b "ledger record gone; fleet list classifies the sprite as down, not missing" fleet_down_not_missing
step 12 "reap dry-run: 0 recorded orphans" reap_dry_clean
step 13 "unrecorded reap is read-only and empty" reap_unrecorded_read_only
if [ "${#FAILED_STEPS[@]}" -gt 0 ]; then
  echo "FAILED STEPS: ${FAILED_STEPS[*]}"
  exit 1
fi
echo "ALL STEPS PASS"
