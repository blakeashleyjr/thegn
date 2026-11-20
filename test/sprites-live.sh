#!/usr/bin/env bash
# Live Sprites run (b160 rx-live). SPENDS REAL MONEY on the owner's Sprites
# account when acting; with no flags it only prints the plan and exits 0.
#
# Neither this nor test/vercel-live.sh is in ci/ci-local/smoke: nothing in the
# justfile, workflows or nix checks globs `test/*live*.sh`; it runs only through
# `just sprites-live`. It reads the owner's REAL config read-only
# (XDG_CONFIG_HOME as given; the sprites token is never read or printed) with a
# fresh XDG_STATE_HOME and THEGN_DIR and a scratch repo. The login runtime dir
# is kept so sandbox masks and the control guard cover every instance's control
# directory.
#
# The daily headless/land/reap run lives in test/remote-daily.sh (just remote-daily).
#
# Usage:
#   test/sprites-live.sh                       # print plan, no network
#   THEGN_CHANNEL=dev test/sprites-live.sh --env <name> --i-accept-spend [--keep-going]
#     [--with-agent <name>]
set -euo pipefail

THEGN_BIN="${THEGN_BIN:-thegn}"

env_name="" accept=0 keep_going=0 min_ram="" agent_name=""
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
  --with-agent)
    agent_name="${2:?--with-agent needs a value}"
    case "$agent_name" in
    '' | *[!a-z0-9_-]*)
      echo "--with-agent needs a value matching [a-z0-9_-]+" >&2
      exit 2
      ;;
    esac
    shift 2
    ;;
  -h | --help)
    sed -n 2,16p "$0"
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

print_plan() {
  cat <<PLAN
sprites-live plan (nothing is created without both --env <name> and
--i-accept-spend, plus THEGN_CHANNEL=dev)

Will create : ONE sprite via the named [env.<name>] from your real config
              (read-only), in a fresh XDG_STATE_HOME and THEGN_DIR (login runtime dir kept) and a scratch git
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
  1   scratch repo + env test (with the requested --set when given)
      1b  env test
  2   no sprites ledger record before create
  3   provision (env up, with the requested --set when given)
      3b  exactly one ready sprites ledger record
  4   sprite workspace HEAD == local HEAD (sprite-exec, \$HOME/workspace)
  4b  sprite-exec exit status and stderr reach the caller (exit 3, boom)
  5   (only with --min-ram-gb N) doctor --json provider_cache.<name>.sprites_memory_limit_mb shows N*1024
  5b  (only with --min-ram-gb N) INFO: the guest's MemTotal from /proc/meminfo
      (never FAIL; the policy is not a guest cgroup)
  5c  (only with --min-ram-gb N) fleet inspect --policy reads back N*1024 MiB
  5d  (only with --min-ram-gb N; runs after 10x) a guest process allocates N*1024+512 MiB
      under a 120 s timeout: PASS when it is stopped at the limit; WARN (not FAIL) when it
      succeeds: the limit is accepted by the API but not enforced in the guest
  6   interactive exec under script(1): /dev/pts/<n> and ok; print the trace
      log path and the last 40 "sprites exec frame" lines
  6c  interactive output line breaks: WARN (not FAIL) when /dev/pts/<n> and ok
      share a line: the Sprites server sends the tty line without its line
      ending (THE-2450); thegn forwards bytes as received
  6b  curl exists in the sprite (keep-awake precondition)
  7a  headless hold: sprite-exec --hold 10m -- sleep 420 in the background (no
      per-run overlay can add a hold-test agent, so no agent run --wait);
      /v1/tasks shows thegn-hold- during, none after
  7e  the hold's stderr has no "keep-awake unavailable"
  8   env pull after an empty commit inside the sprite; output is fast-forwarded
  8b  local HEAD equals the commit made inside the sprite
  8c  refs/thegn/pull is empty after the clean fast-forward
  8d  env snapshot creates a checkpoint
  8e  env snapshots lists it
  8f  env restore of it exits 0 (ledger proof, health check, stream completion)
  7g  (only with --with-agent <name>) agent run --wait --pull --json spends model usage;
      poll /v1/tasks every 15 s while it runs
      (on failure prints the agent's status, exit code and error, never secrets)
  7h  (only with --with-agent <name>) agent JSON is exited 0 and pull.ok is true
  7i  (only with --with-agent <name>) /v1/tasks includes thegn-hold- during the run
      (7g first starts this run's own pane daemon on the fresh state; teardown stops it)
  9   after 120 s idle, fleet list --env shows warm or cold
  9b  /v1/tasks is empty
  10  fleet cost --json: print sprites_usage; error fails, cpu_hours > 0 passes,
      or zero usage (partial or not) warns and passes; reported false warns
  10b fleet list --env remains in the pre-cost warm/cold class (wait up to 90 s
      first if the pre-cost row is running; timeout warns; usage did not wake it)
      (reads the state up to 4 times, 10 s apart: fails only if it stays running; warns on provider noise)
  10u (INFO) direct usage reads: state, account usage GET, state, per-sprite usage GET, state, 10 s, state;
      shows whether a Sprites usage read is followed by a "running" reading (THE-2476)
  10w wake from cold: sprite-exec -- true completes within 150 s; print wake time
  10x wake row is running or warm (a lagging cold state warns and passes)
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

MIN_RAM_SET=()
if [ -n "$min_ram" ]; then
  MIN_RAM_SET=(--set "env.$env_name.provider.min_ram_gb=$min_ram")
fi
# shellcheck disable=SC2034 # read by test/lib/live.sh
LIVE_THEGN_ARGS=("${MIN_RAM_SET[@]+"${MIN_RAM_SET[@]}"}")

# ---- step 0: preflight (doctor's "Remote live readiness" block) ----
MEMPROBE_PY='
import sys
target = int(sys.argv[2])
try:
    with open("/proc/self/oom_score_adj", "w") as f:
        f.write("1000")
except OSError:
    pass
avail = None
try:
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith("MemAvailable:"):
                avail = int(line.split()[1]) // 1024
except (OSError, ValueError):
    pass
if avail is None:
    print("avail unknown", flush=True)
else:
    print("avail", avail, flush=True)
held = []
reached = 0
try:
    while reached < target:
        held.append(b"\x01" * (64 << 20))
        reached += 64
        print("reached", reached, flush=True)
except MemoryError:
    print("memoryerror", reached, flush=True)
    sys.exit(3)
print("done", reached, flush=True)
'
preflight_capture() { live_preflight_capture; }
preflight_check() { live_preflight_check "$@"; }
# Run against the owner's real state, before the fresh dirs below, so
# pre-existing unbound sandboxes are listed.
preflight_capture

# Real config stays as given (read-only); everything mutable is fresh.
TMP="$(mktemp -d)"
export XDG_STATE_HOME="$TMP/state" THEGN_DIR="$TMP/thegn"
unset THEGN_CONFIG_DIR
mkdir -p "$XDG_STATE_HOME" "$THEGN_DIR" "$TMP/repo"
REPO="$TMP/repo"
SPRITE_NAME="(unknown)"
CREATED=0
HOLD_PID=""
AGENT_POLL_PID=""
DAEMON_PID=""
HOLD_ERR="$TMP/hold.err"
STEP_11_PASSED=0
# shellcheck disable=SC2034 # appended to and read by test/lib/live.sh
FAILED_STEPS=()
INTERACTIVE_OUT=""
PULL_SHA=""
CHECKPOINT_ID=""
AGENT_JSON=""
AGENT_RUN_RC=0
AGENT_STDERR_FILE=""
AGENT_RUN_SECONDS=0
COST_PRE_STATE=""
COST_PRE_WAIT_TIMED_OUT=0
# shellcheck disable=SC2034 # read by test/lib/live.sh
LIVE_STARTUP=("3=cold create (env up)" "6=first interactive output (sprite-exec under script)" "10w=wake from cold (sprite-exec -- true)")

live_on_pass() { if [ "$1" = 11 ]; then STEP_11_PASSED=1; fi; }

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
  out="$("$THEGN_BIN" "${MIN_RAM_SET[@]}" fleet list --env "$env_name" --json 2>/dev/null)" || return 1
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
  if [ -n "$AGENT_POLL_PID" ] && kill -0 "$AGENT_POLL_PID" 2>/dev/null; then
    echo "-- teardown: stopping the agent task poller (pid $AGENT_POLL_PID)"
    kill "$AGENT_POLL_PID" 2>/dev/null
    wait "$AGENT_POLL_PID" 2>/dev/null
    AGENT_POLL_PID=""
  fi
  if [ "$CREATED" = 1 ]; then
    local names n
    names="$(run_records sprites)"
    if [ "$STEP_11_PASSED" = 0 ]; then
      echo "-- teardown: env down"
      (cd "$REPO" && "$THEGN_BIN" "${MIN_RAM_SET[@]}" env down --force)
    fi
    for n in $names; do
      case "$n" in *[!a-z0-9-]*) continue ;; esac
      echo "-- teardown: env deprovision $n"
      if ! "$THEGN_BIN" "${MIN_RAM_SET[@]}" env deprovision "$n" --env "$env_name"; then
        if ! listing_lacks_name "$n"; then
          (cd "$REPO" && "$THEGN_BIN" "${MIN_RAM_SET[@]}" env down --worktree "$REPO" --force)
          if case "$(live_listing "$env_name" "$n")" in present\ *) true ;; *) false ;; esac; then
            echo "-- teardown: sandbox $n still exists; remove it with thegn env reap"
          fi
        fi
      fi
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
  if [ -n "$DAEMON_PID" ]; then
    echo "-- teardown: daemon stop (this run's pane daemon, pid $DAEMON_PID)"
    "$THEGN_BIN" daemon stop
    kill "$DAEMON_PID" 2>/dev/null
    local daemon_tries=0
    while [ "$daemon_tries" -lt 10 ] && kill -0 "$DAEMON_PID" 2>/dev/null; do
      sleep 1
      daemon_tries=$((daemon_tries + 1))
    done
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
      echo "-- teardown: pane daemon $DAEMON_PID ignored TERM; sending KILL" >&2
      kill -KILL "$DAEMON_PID" 2>/dev/null
    fi
    wait "$DAEMON_PID" 2>/dev/null
    DAEMON_PID=""
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
  live_step "$@"
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

exec_exit_and_stderr() {
  local out err rc
  err="$TMP/4b.stderr"
  set +e
  out="$(no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- sh -c 'echo boom >&2; exit 3' 2>"$err")"
  rc=$?
  set -e
  printf '%s\n' "$out"
  if [[ $out == *boom* ]]; then
    echo "   stderr marker arrived on stdout"
  elif grep -Fq boom "$err"; then
    sed 's/^/   /' "$err"
    echo "   stderr marker arrived on stderr"
  else
    return 1
  fi
  [ "$rc" -eq 3 ]
}

memory_reported() { # INFO only: the guest's MemTotal; the Sprites policy is not a guest cgroup
  local out
  if ! out="$(no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- sh -c 'grep "^MemTotal:" /proc/meminfo')"; then
    echo "INFO 5b: could not read /proc/meminfo in the sprite"
    return 0
  fi
  out="$(printf '%s' "$out" | tr -d '\r')"
  echo "INFO 5b: guest $out (the memory policy is read back in 5c, not from the guest)"
  return 0
}

policy_read_back() { # fleet inspect --policy reflects min_ram_gb=N as N*1024 MiB
  local err
  err="$TMP/5c.stderr"
  if ! "$THEGN_BIN" "${MIN_RAM_SET[@]}" fleet inspect "$SPRITE_NAME" --env "$env_name" --policy --json >"$TMP/5c.json" 2>"$err"; then
    echo "fleet inspect stderr:" >&2
    sed 's/^/  /' "$err" >&2
    return 1
  fi
  if ! MIN_RAM_EXPECTED="$((min_ram * 1024))" python3 -I -c '
import json, os, sys
try:
    report = json.load(sys.stdin)
    got = report["memory_limit_mb"]
except (KeyError, TypeError, ValueError) as exc:
    print("fleet inspect JSON is missing memory_limit_mb: " + str(exc), file=sys.stderr)
    sys.exit(1)
expected = int(os.environ["MIN_RAM_EXPECTED"])
if got != expected:
    print("fleet inspect JSON reports memory_limit_mb=" + str(got) + ", want " + str(expected), file=sys.stderr)
    sys.exit(1)
print("   fleet inspect memory policy: " + str(got) + " MiB")
' <"$TMP/5c.json"; then
    echo "fleet inspect stderr:" >&2
    sed 's/^/  /' "$err" >&2
    return 1
  fi
}

memory_enforced() {
  local out rc limit target verdict verdict_rc cgroup probe_info
  if ! no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- sh -c 'command -v python3 && command -v timeout'; then
    echo "WARN 5d: python3 or timeout is missing in the guest; memory enforcement unproven"
    return 0
  fi
  if cgroup="$(no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- sh -c 'cat /sys/fs/cgroup/memory.max 2>/dev/null || echo absent')"; then
    printf '%s\n' "$cgroup" | tr -d '\r' | sed 's/^/INFO 5d: guest cgroup memory.max: /'
  else
    echo "INFO 5d: could not read the guest cgroup memory.max"
  fi
  limit=$((min_ram * 1024))
  target=$((limit + 512))
  out="$(timeout 300 "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- timeout 120 python3 -I -c "$MEMPROBE_PY" thegn-memprobe "$target" "$target" </dev/null 2>"$TMP/5d.stderr")" && rc=0 || rc=$?
  verdict=""
  verdict_rc=0
  if ! verdict="$({
    MEMPROBE_OUT="$out" MEMPROBE_RC="$rc" MEMPROBE_LIMIT="$limit" MEMPROBE_TARGET="$target" \
      python3 -I -c '
import os
import re

out = os.environ["MEMPROBE_OUT"]
rc = int(os.environ["MEMPROBE_RC"])
limit = int(os.environ["MEMPROBE_LIMIT"])
target = int(os.environ["MEMPROBE_TARGET"])
lines = out.replace("\r", "").splitlines()
avail = [line for line in lines if re.fullmatch(r"avail (?:unknown|\d+)", line)]
if avail:
    print(avail[-1])
else:
    print("avail unknown")
if rc == 124:
    print("timeout 0")
else:
    done = [int(m.group(1)) for line in lines if (m := re.fullmatch(r"done (\d+)", line))]
    reached = [int(m.group(1)) for line in lines if (m := re.fullmatch(r"reached (\d+)", line))]
    errors = [int(m.group(1)) for line in lines if (m := re.fullmatch(r"memoryerror (\d+)", line))]
    stopped = reached[-1] if reached else (errors[-1] if errors else 0)
    was_stopped = rc in (137, 3) or bool(errors)
    if rc == 0 and done:
        print("unenforced " + str(done[-1]))
    elif was_stopped and stopped < limit // 2:
        print("early " + str(stopped))
    elif was_stopped and stopped <= limit + 256:
        print("enforced " + str(stopped))
    elif was_stopped and stopped > limit + 256:
        print("overshoot " + str(stopped))
    else:
        print("noout 0")
'
  })"; then
    verdict="noout 0"
    verdict_rc=1
  fi
  probe_info="${verdict%%$'\n'*}"
  case "$probe_info" in
  "avail unknown")
    echo "INFO 5d: guest MemAvailable: unknown"
    ;;
  "avail "*)
    echo "INFO 5d: guest MemAvailable: ${probe_info#avail } MiB (policy ${limit} MiB)"
    ;;
  *)
    echo "INFO 5d: guest MemAvailable: unknown"
    ;;
  esac
  if [ "$probe_info" != "$verdict" ]; then
    verdict="${verdict#*$'\n'}"
  else
    verdict="noout 0"
  fi
  case "$verdict" in
  timeout\ *)
    echo "WARN 5d: the memory probe timed out after 120 s; memory enforcement unproven"
    ;;
  unenforced\ *)
    echo "WARN 5d: limit accepted by the API but not enforced in the guest: one process held ${verdict#unenforced } MiB under a ${limit} MiB limit (Sprites provider behaviour, not a thegn failure)"
    ;;
  enforced\ *)
    echo "   memory limit enforced: the probe was stopped at ${verdict#enforced } MiB (limit ${limit} MiB, rc ${rc})"
    ;;
  early\ *)
    echo "WARN 5d: the probe was stopped early at ${verdict#early } MiB (limit ${limit} MiB); inconclusive"
    ;;
  overshoot\ *)
    echo "WARN 5d: the probe was stopped at ${verdict#overshoot } MiB, above the ${limit} MiB limit; the limit is not enforced at its stated value"
    ;;
  noout\ *)
    printf '%s\n' "$out" | sed 's/^/  /'
    sed 's/^/  /' "$TMP/5d.stderr"
    verdict_rc=1
    ;;
  esac
  if ! no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- true; then
    echo "the sprite did not answer after the memory probe" >&2
    return 1
  fi
  return "$verdict_rc"
}

curl_present() { # the keep-awake hold needs curl inside the sprite
  no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- sh -c 'command -v curl'
}

min_ram_shown() { # doctor reflects min_ram_gb=N as N*1024 MiB
  local out err
  err="$TMP/doctor-min-ram.stderr"
  if ! out="$("$THEGN_BIN" "${MIN_RAM_SET[@]}" doctor --json 2>"$err")"; then
    echo "doctor stderr:" >&2
    sed 's/^/  /' "$err" >&2
    return 1
  fi
  if ! printf '%s' "$out" | MIN_RAM_EXPECTED="$((min_ram * 1024))" MIN_RAM_ENV="$env_name" python3 -I -c '
import json, os, sys
try:
    report = json.load(sys.stdin)
    got = report["provider_cache"][os.environ["MIN_RAM_ENV"]]["sprites_memory_limit_mb"]
except (KeyError, TypeError, ValueError) as exc:
    print("doctor JSON is missing provider_cache.<env>.sprites_memory_limit_mb: " + str(exc), file=sys.stderr)
    sys.exit(1)
expected = int(os.environ["MIN_RAM_EXPECTED"])
if got != expected:
    print("doctor JSON reports sprites_memory_limit_mb=" + str(got) + ", want " + str(expected), file=sys.stderr)
    sys.exit(1)
print("   doctor provider_cache memory: " + str(got) + " MiB")
'; then
    echo "doctor stderr:" >&2
    sed 's/^/  /' "$err" >&2
    return 1
  fi
}

interactive_tty() { # values reach script(1) via the environment, never the command text
  local log_path frame_log_tail frame_count
  # shellcheck disable=SC2016 # fixed text; script(1)'s shell expands the env values
  INTERACTIVE_OUT="$(LIVE_BIN="$THEGN_BIN" LIVE_SPRITE="$SPRITE_NAME" SHELL=/bin/sh \
    THEGN_LOG=thegn::provider=trace THEGN_TRACE_FILE="$TMP/sprite-exec-trace.log" \
    script -qec 'exec "$LIVE_BIN" sprite-exec "$LIVE_SPRITE" -- sh -c "tty && echo ok"' /dev/null </dev/null)" || return 1
  printf '%s\n' "$INTERACTIVE_OUT"
  log_path="$TMP/sprite-exec-trace.log"
  frame_log_tail=""
  frame_count=0
  if [ -f "$log_path" ]; then
    frame_count="$(grep -Fc 'sprites exec frame' "$log_path" || true)"
    frame_log_tail="$(grep -F 'sprites exec frame' "$log_path" | tail -n 40)" || true
  fi
  echo "   trace log path: $log_path"
  if [ "$frame_count" -eq 0 ]; then
    echo "   trace: none (binary predates the bridge trace)"
  else
    echo "   trace lines: $frame_count"
    echo "   last 40 sprites exec frame lines:"
    printf '%s\n' "$frame_log_tail"
  fi
  grep -Eq '/dev/pts/[0-9]+' <<<"$INTERACTIVE_OUT" && grep -q 'ok' <<<"$INTERACTIVE_OUT"
}

interactive_line_breaks() {
  case "$(INTERACTIVE_OUT="$INTERACTIVE_OUT" python3 -I -c '
import os, re, sys
lines = os.environ["INTERACTIVE_OUT"].replace("\r", "").splitlines()
tty = next((i for i, line in enumerate(lines) if re.search(r"/dev/pts/[0-9]+", line)), None)
# The server can send the tty path with no line ending, so "ok" may follow it directly ("/dev/pts/0ok").
ok = next((i for i, line in enumerate(lines) if re.search(r"(^|\s|/dev/pts/[0-9]+)ok(\s|$)", line)), None)
if tty is None or ok is None:
    print("fail")
elif tty != ok:
    print("pass")
else:
    print("warn")
')" in
  pass)
    return 0
    ;;
  warn)
    echo "WARN 6c: /dev/pts/<n> and ok arrived on one line; this is the Sprites server's output (THE-2450, upstream), not a thegn failure"
    printf '%s\n' "$INTERACTIVE_OUT" | cat -v
    return 0
    ;;
  *)
    return 1
    ;;
  esac
}

sprites_api_get() { # the sprite's in-VM API; the second arg selects body or status output
  local path="$1" mode="${2:-body}"
  case "$mode" in
  body)
    no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- \
      sh -c 'curl -sS --unix-socket /.sprite/api.sock "http://sprite$1"' sh "$path"
    ;;
  status)
    no_stdin "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- \
      sh -c 'curl -sS -o /dev/null -w "%{http_code}" --unix-socket /.sprite/api.sock "http://sprite$1"' sh "$path"
    ;;
  *)
    return 1
    ;;
  esac
}
list_tasks() { # the sprite's in-VM Tasks API
  sprites_api_get /v1/tasks
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
  out="$(in_repo "$THEGN_BIN" "${MIN_RAM_SET[@]}" env pull 2>&1)" || {
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

checkpoint_create() {
  local line candidate
  if ! in_repo "$THEGN_BIN" "${MIN_RAM_SET[@]}" env snapshot --label sx-live >"$TMP/8d.out" 2>"$TMP/8d.err"; then
    echo "env snapshot stderr:" >&2
    sed 's/^/  /' "$TMP/8d.err" >&2
    return 1
  fi
  CHECKPOINT_ID=""
  while IFS= read -r line; do
    case "$line" in
    "created checkpoint: "*)
      candidate="${line#created checkpoint: }"
      case "$candidate" in
      '' | *[!A-Za-z0-9_.-]*) ;;
      *)
        CHECKPOINT_ID="$candidate"
        break
        ;;
      esac
      ;;
    esac
  done <"$TMP/8d.out"
  if [ -z "$CHECKPOINT_ID" ]; then
    echo "env snapshot did not print a valid checkpoint id:" >&2
    sed 's/^/  /' "$TMP/8d.err" >&2
    return 1
  fi
  echo "   checkpoint: $CHECKPOINT_ID"
}

checkpoint_listed() {
  local line err="$TMP/8e.err" out
  if [ -z "$CHECKPOINT_ID" ]; then
    echo "checkpoint id is empty:" >&2
    if [ -f "$err" ]; then sed 's/^/  /' "$err" >&2; fi
    return 1
  fi
  if ! out="$(in_repo "$THEGN_BIN" "${MIN_RAM_SET[@]}" env snapshots 2>"$err")"; then
    echo "env snapshots stderr:" >&2
    sed 's/^/  /' "$err" >&2
    return 1
  fi
  while IFS= read -r line; do
    case "$line" in
    "$CHECKPOINT_ID" | "$CHECKPOINT_ID "*)
      echo "   listed checkpoint: $CHECKPOINT_ID"
      return 0
      ;;
    esac
  done <<<"$out"
  echo "env snapshots did not list checkpoint $CHECKPOINT_ID:" >&2
  sed 's/^/  /' "$err" >&2
  return 1
}

checkpoint_restored() {
  local err="$TMP/8f.err" out
  if [ -z "$CHECKPOINT_ID" ]; then
    echo "checkpoint id is empty:" >&2
    if [ -f "$err" ]; then sed 's/^/  /' "$err" >&2; fi
    return 1
  fi
  if ! out="$(in_repo "$THEGN_BIN" "${MIN_RAM_SET[@]}" env restore "$CHECKPOINT_ID" 2>"$err")"; then
    echo "env restore stderr:" >&2
    sed 's/^/  /' "$err" >&2
    return 1
  fi
  printf '%s\n' "$out"
  case "$out" in
  *restored*) return 0 ;;
  esac
  echo "env restore did not print restored:" >&2
  sed 's/^/  /' "$err" >&2
  return 1
}

warm_or_cold() {
  local state
  state="$(fleet_row_state)" || return 1
  case "$state" in
  warm | cold)
    echo "   sprite state: $state"
    ;;
  *)
    echo "sprite row state is not warm or cold: $state" >&2
    return 1
    ;;
  esac
}

fleet_row_state() {
  local answer
  answer="$(live_listing "$env_name" "$SPRITE_NAME")" || return 1
  case "$answer" in
  "present -")
    echo "sprite row has no state" >&2
    return 1
    ;;
  "present "*)
    printf '%s\n' "${answer#present }"
    ;;
  *)
    echo "sprite row is not in the listing ($answer)" >&2
    return 1
    ;;
  esac
}
idle_warm_or_cold() {
  echo "   idling 120 s"
  sleep 120
  warm_or_cold
}

wake_from_cold() {
  local started ended elapsed
  started="$(date +%s)" || return 1
  timeout 150 "$THEGN_BIN" sprite-exec "$SPRITE_NAME" -- true </dev/null || return 1
  ended="$(date +%s)" || return 1
  elapsed=$((ended - started))
  echo "   woke in ${elapsed} s"
}

wake_state() {
  local out
  out="$("$THEGN_BIN" "${MIN_RAM_SET[@]}" fleet list --env "$env_name" --json)" || return 1
  FLEET_JSON="$out" FLEET_SPRITE="$SPRITE_NAME" python3 -I -c '
import json, os, sys
try:
    listing = json.loads(os.environ["FLEET_JSON"])
    rows = listing["view"]["rows"]
except (KeyError, TypeError, ValueError) as exc:
    print("fleet list JSON is invalid: " + str(exc), file=sys.stderr)
    sys.exit(1)
if not isinstance(rows, list):
    print("fleet list JSON view.rows is not an array", file=sys.stderr)
    sys.exit(1)
row = next((r for r in rows if isinstance(r, dict) and r.get("id") == os.environ["FLEET_SPRITE"]), None)
if row is None:
    print("sprite row absent from the listing", file=sys.stderr)
    sys.exit(1)
state = row.get("state")
if state in {"running", "warm"}:
    print("   wake state: " + state)
    sys.exit(0)
if state == "cold":
    print("WARN 10x: sprite state is still cold; state lags the wake", file=sys.stderr)
    sys.exit(0)
print("sprite row state is not running, warm, or cold: " + str(state), file=sys.stderr)
sys.exit(1)
'
}

agent_tasks_poll() {
  : >"$TMP/agent-tasks.log"
  while :; do
    list_tasks >>"$TMP/agent-tasks.log" 2>&1 || true
    sleep 15
  done
}

# agent run connects to a pane daemon and never spawns one, and this run's
# state is fresh: start this run's own daemon (same binary, same state) in
# the background; teardown stops it. THEGN_CONTROL_ADDR is unset so neither
# the probe nor agent run can reach another instance's daemon.
agent_daemon_start() {
  local tries=0
  [ -z "$DAEMON_PID" ] || return 0
  unset THEGN_CONTROL_ADDR
  "$THEGN_BIN" "${MIN_RAM_SET[@]}" daemon </dev/null >"$TMP/daemon.log" 2>&1 &
  DAEMON_PID=$!
  while [ "$tries" -lt 30 ]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
      echo "pane daemon exited at start; log: $TMP/daemon.log" >&2
      tail -n 20 "$TMP/daemon.log" >&2
      return 1
    fi
    if "$THEGN_BIN" session list --json >/dev/null 2>&1; then
      echo "   pane daemon up (pid $DAEMON_PID)"
      return 0
    fi
    sleep 1
    tries=$((tries + 1))
  done
  echo "pane daemon did not answer session list within 30 s; log: $TMP/daemon.log" >&2
  return 1
}

agent_run() {
  local started ended rc
  agent_daemon_start || return 1
  started="$(date +%s)" || return 1
  agent_tasks_poll &
  AGENT_POLL_PID=$!
  set +e
  AGENT_STDERR_FILE="$TMP/agent-run.stderr"
  AGENT_JSON="$(in_repo "$THEGN_BIN" agent run --worktree "$REPO" --agent "$agent_name" --prompt "create sx-agent.txt containing ok and commit it" --wait --pull --json 2>"$AGENT_STDERR_FILE")"
  rc=$?
  kill "$AGENT_POLL_PID" 2>/dev/null
  wait "$AGENT_POLL_PID" 2>/dev/null
  AGENT_POLL_PID=""
  set -e
  AGENT_RUN_RC=$rc
  ended="$(date +%s)" || return 1
  AGENT_RUN_SECONDS=$((ended - started))
  if [ "$rc" -ne 0 ]; then
    agent_failure_summary "$rc"
    return "$rc"
  fi
  [ -n "$AGENT_JSON" ]
}

agent_failure_summary() {
  local status="${1:-$AGENT_RUN_RC}" stderr_file="${AGENT_STDERR_FILE:-$TMP/agent-run.stderr}"
  AGENT_SUMMARY_JSON="$AGENT_JSON" AGENT_SUMMARY_STATUS="$status" AGENT_SUMMARY_STDERR="$stderr_file" python3 -I -c '
import json, os, sys

def clean(value):
    if isinstance(value, str):
        text = value
    elif value is None or isinstance(value, (bool, int, float)):
        text = json.dumps(value)
    else:
        text = type(value).__name__
    text = "".join(ch for ch in text if ord(ch) >= 32 and not (127 <= ord(ch) <= 159))
    return text[:200]

def emit(label, value):
    print("   agent " + label + ": " + clean(value))

emit("command exit status", os.environ.get("AGENT_SUMMARY_STATUS", "unknown"))
raw = os.environ.get("AGENT_SUMMARY_JSON", "")
try:
    result = json.loads(raw)
except Exception:
    if raw:
        emit("JSON", "invalid")
else:
    if isinstance(result, dict):
        for key in ("status", "exit_code", "error"):
            if key in result:
                emit(key, result[key])
        if "provision" in result:
            provision = result["provision"]
            if isinstance(provision, (dict, list)):
                provision = type(provision).__name__
            emit("provision", provision)
        pull = result.get("pull")
        if isinstance(pull, dict):
            if "ok" in pull:
                emit("pull.ok", pull["ok"])
            if "error" in pull:
                emit("pull.error", pull["error"])
        for key in ("stderr", "output"):
            value = result.get(key)
            if isinstance(value, str):
                lines = value.splitlines()[-20:]
                for line in lines:
                    emit(key, line)
        if isinstance(result.get("sandbox"), str):
            emit("sandbox", result["sandbox"])
        if "output_tail" in result:
            tail = result["output_tail"]
            if isinstance(tail, str):
                lines = tail.splitlines()[-20:]
                print("   agent output tail:")
                for line in lines:
                    print("      " + clean(line))
            elif isinstance(tail, list) and all(isinstance(line, str) for line in tail):
                lines = "\n".join(tail).splitlines()[-20:]
                print("   agent output tail:")
                for line in lines:
                    print("      " + clean(line))
            else:
                print("   agent output tail: " + type(tail).__name__)

path = os.environ.get("AGENT_SUMMARY_STDERR", "")
if path:
    try:
        with open(path, encoding="utf-8", errors="replace") as stream:
            lines = stream.read().splitlines()[-20:]
    except OSError:
        lines = []
    for line in lines:
        emit("stderr", line)
'
  return 0
}

agent_result_ok() {
  if AGENT_RESULT_JSON="$AGENT_JSON" python3 -I -c '
import json, os, sys
try:
    result = json.loads(os.environ["AGENT_RESULT_JSON"])
except (TypeError, ValueError) as exc:
    print("agent run JSON is invalid: " + str(exc), file=sys.stderr)
    sys.exit(1)
if result.get("status") != "exited":
    print("agent run status is not exited", file=sys.stderr)
    sys.exit(1)
if result.get("exit_code") != 0:
    print("agent run exit_code is not 0", file=sys.stderr)
    sys.exit(1)
pull = result.get("pull")
if not isinstance(pull, dict) or pull.get("ok") is not True:
    print("agent run pull.ok is not true", file=sys.stderr)
    sys.exit(1)
'
  then
    return 0
  fi
  agent_failure_summary
  return 1
}

agent_task_log_ok() {
  if grep -Fq 'thegn-hold-' "$TMP/agent-tasks.log" 2>/dev/null; then
    return 0
  fi
  if [ "$AGENT_RUN_SECONDS" -lt 30 ]; then
    echo "WARN 7i: agent run lasted ${AGENT_RUN_SECONDS} s; task poll may have missed thegn-hold-" >&2
    return 0
  fi
  return 1
}

cost_has_cpu() {
  local out pre_state polls
  COST_PRE_WAIT_TIMED_OUT=0
  pre_state="$(fleet_row_state)" || return 1
  polls=0
  while [ "$pre_state" = running ] && [ "$polls" -lt 18 ]; do
    sleep 5
    pre_state="$(fleet_row_state)" || return 1
    polls=$((polls + 1))
  done
  COST_PRE_STATE="$pre_state"
  if [ "$pre_state" = running ]; then
    COST_PRE_WAIT_TIMED_OUT=1
    echo "WARN 10b: sprite row remained running before fleet cost after 90 s"
  else
    echo "   sprite state before fleet cost: $pre_state"
  fi
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
zero_usage = False
zero_reported = False
not_reported = False
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
    if row.get("reported") is False:
        not_reported = True
        continue
    if isinstance(cpu_hours, (int, float)) and cpu_hours > 0:
        continue
    if cpu_hours == 0:
        if row.get("reported") is True:
            zero_reported = True
        elif "reported" not in row:
            zero_usage = True
        else:
            print("fleet cost sprites_usage reported is not a boolean", file=sys.stderr)
            sys.exit(1)
        continue
    print("fleet cost sprites_usage cpu_hours is not positive", file=sys.stderr)
    sys.exit(1)
if not any(block.get("env") == os.environ["COST_ENV"] for block in usage):
    print("fleet cost sprites_usage has no block for env " + os.environ["COST_ENV"], file=sys.stderr)
    sys.exit(1)
if zero_usage:
    print("WARN 10: usage not reported yet for a sprite this young")
if zero_reported:
    print("WARN 10: the Sprites API reported zero CPU for a sprite this young")
if not_reported:
    print("WARN 10: no usage reported yet (the Sprites usage API has no record for this sprite yet)")
'
}

cost_state_ok() {
  local after err="$TMP/10b.err" rc reads=0 running_reads=0 first_success=0
  while [ "$reads" -lt 4 ]; do
    : >"$err"
    after="$(fleet_row_state 2>"$err")" && rc=0 || rc=$?
    reads=$((reads + 1))
    if [ "$rc" -eq 0 ]; then
      if [ "$first_success" -eq 0 ]; then
        echo "   sprite state after fleet cost: $after"
        first_success=1
      fi
      if [ "$COST_PRE_WAIT_TIMED_OUT" = 1 ]; then
        echo "WARN 10b: comparing after-cost state against timed-out pre-cost state $COST_PRE_STATE"
        return 0
      fi
      case "$after" in
      warm | cold)
        if [ "$running_reads" -gt 0 ]; then
          echo "WARN 10b: the sprite read running $running_reads time(s) after fleet cost before settling to $after"
        fi
        return 0
        ;;
      running)
        running_reads=$((running_reads + 1))
        ;;
      *)
        printf '%s\n' "sprite row state is not warm, cold, or running: $after" >"$err"
        ;;
      esac
    fi
    if [ "$reads" -lt 4 ]; then
      sleep 10
    fi
  done
  if [ "$running_reads" -gt 0 ]; then
    echo "the sprite stayed running after fleet cost (a read woke it); it read running $running_reads of $reads time(s) and never warm or cold" >&2
    return 1
  fi
  echo "WARN 10b: the provider listing could not be read after fleet cost; the wake check is unproven"
  if [ -s "$err" ]; then
    sed 's/^/   /' "$err"
  fi
  return 0
}

usage_probe_state() {
  local state rc
  state="$(fleet_row_state 2>/dev/null)" && rc=0 || rc=$?
  if [ "$rc" -ne 0 ] || [ -z "$state" ]; then
    state=unknown
  fi
  printf '%s\n' "$state"
}

usage_probe_code() {
  local path="$1" code rc
  code="$(sprites_api_get "$path" status)" && rc=0 || rc=$?
  case "$rc:$code" in
  0:[0-9][0-9][0-9]) printf '%s\n' "$code" ;;
  *) printf '%s\n' unknown ;;
  esac
}

usage_state_probe() {
  local start state_a state_b state_c state_d account_code sprite_code rc
  start="$(date -u '+%Y-%m-01T00:00:00Z')" && rc=0 || rc=$?
  if [ "$rc" -ne 0 ] || [ -z "$start" ]; then
    echo "INFO 10u: state before usage reads: unknown"
    echo "INFO 10u: after the account usage read (HTTP unknown): unknown"
    echo "INFO 10u: after the per-sprite usage read (HTTP unknown): unknown"
    echo "INFO 10u: 10 s later: unknown"
    echo "INFO 10u: no usage read changed the reported state"
    return 0
  fi
  start="${start//:/%3A}"
  state_a="$(usage_probe_state)"
  echo "INFO 10u: state before usage reads: $state_a"
  account_code="$(usage_probe_code "/v1/usage?start=$start")"
  sleep 3
  state_b="$(usage_probe_state)"
  echo "INFO 10u: after the account usage read (HTTP $account_code): $state_b"
  sprite_code="$(usage_probe_code "/v1/usage/$SPRITE_NAME?start=$start")"
  sleep 3
  state_c="$(usage_probe_state)"
  echo "INFO 10u: after the per-sprite usage read (HTTP $sprite_code): $state_c"
  sleep 10
  state_d="$(usage_probe_state)"
  echo "INFO 10u: 10 s later: $state_d"
  if [ "$state_c" = running ] && [ "$state_b" != running ] && [ "$state_b" != unknown ]; then
    echo "INFO 10u: the per-sprite usage read is followed by a running reading"
  elif [ "$state_b" = running ] && [ "$state_a" != running ] && [ "$state_a" != unknown ]; then
    echo "INFO 10u: the account usage read is followed by a running reading"
  else
    echo "INFO 10u: no usage read changed the reported state"
  fi
  return 0
}

fleet_down_not_missing() {
  local out
  [ ! -f "$(ledger_dir)/$SPRITE_NAME.json" ] || {
    echo "   ledger record for $SPRITE_NAME is still present" >&2
    return 1
  }
  out="$("$THEGN_BIN" "${MIN_RAM_SET[@]}" fleet list --env "$env_name" --json)" || return 1
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
step 1b "env test" "$THEGN_BIN" "${MIN_RAM_SET[@]}" env test "$env_name"
step 2 "no sprites ledger record before create" ledger_empty
in_repo "$THEGN_BIN" "${MIN_RAM_SET[@]}" env set "$env_name" >/dev/null
CREATED=1
step 3 "provision sprite (env up)" in_repo "$THEGN_BIN" "${MIN_RAM_SET[@]}" env up
step 3b "exactly one ready sprites ledger record (plan step 3)" name_from_ledger
echo "sprite: $SPRITE_NAME"
step 4 "sprite workspace HEAD matches the local lane HEAD" head_matches
step 4b "sprite-exec exit status and stderr reach the caller" exec_exit_and_stderr
if [ -n "$min_ram" ]; then
  step 5 "doctor shows min_ram_gb=$min_ram as $((min_ram * 1024)) MiB" min_ram_shown
  step 5b "guest MemTotal (INFO only)" memory_reported
  step 5c "memory policy reads back $((min_ram * 1024)) MiB (fleet inspect --policy)" policy_read_back
else
  echo "SKIP 5, 5b, 5c, 5d (needs --min-ram-gb N)"
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
step 8d "env snapshot creates a checkpoint" checkpoint_create
step 8e "env snapshots lists it" checkpoint_listed
step 8f "env restore of it exits 0 (ledger proof, health check, stream completion)" checkpoint_restored
if [ -n "$agent_name" ]; then
  step 7g "agent run --wait --pull --json (spends model usage)" agent_run
  step 7h "agent JSON is exited 0 with pull.ok true" agent_result_ok
  step 7i "agent task polling sees thegn-hold-" agent_task_log_ok
else
  echo "SKIP 7g-7i (needs --with-agent <name>)"
fi
step 9 "after 120 s idle the sprite is warm or cold" idle_warm_or_cold
step 9b "/v1/tasks is empty" tasks_empty
step 10 "fleet cost reports sprites_usage CPU hours or zero usage (partial or not)" cost_has_cpu
step 10b "fleet list remains in the pre-cost warm/cold class after fleet cost" cost_state_ok
step 10u "direct usage reads report whether a usage read is followed by running" usage_state_probe
step 10w "wake from cold (sprite-exec -- true within 150 s)" wake_from_cold
step 10x "wake row is running or warm" wake_state
if [ -n "$min_ram" ]; then
  step 5d "a guest allocation past $((min_ram * 1024)) MiB is stopped (WARN when the limit is not enforced)" memory_enforced
fi
step 11 "env down destroys the sprite" in_repo "$THEGN_BIN" "${MIN_RAM_SET[@]}" env down
step 11b "ledger record gone; fleet list classifies the sprite as down, not missing" fleet_down_not_missing
step 12 "reap dry-run: 0 recorded orphans" reap_dry_clean
step 13 "unrecorded reap is read-only and empty" reap_unrecorded_read_only
live_finish
