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
#   THEGN_CHANNEL=dev test/sprites-live.sh --env <name> --i-accept-spend
set -euo pipefail

THEGN_BIN="${THEGN_BIN:-thegn}"

env_name="" accept=0 min_ram=""
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
              --force, env deprovision <name> for each record in THIS run's
              ledger, env reap --provider sprites --apply --yes; fails (and keeps
              the ledger) if any of this run's records or sprites remain.
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
  6   interactive exec under script(1): tty && echo ok
  6b  curl exists in the sprite (keep-awake precondition)
  7a  headless hold: sprite-exec --hold 10m -- sleep 420 in the background (no
      per-run overlay can add a hold-test agent, so no agent run --wait);
      /v1/tasks shows thegn-hold- during, none after
  7e  the hold's stderr has no "keep-awake unavailable"
  8   env pull                         8b  refs/thegn/pull is non-empty
  9   after 120 s idle, fleet list --env shows warm or cold
  9b  /v1/tasks is empty
  10  fleet cost --json: sprites_usage with CPU seconds > 0
  10b fleet list --env still shows warm or cold (the usage call did not wake it)
  11  env down                         11b this run's ledger record is gone
  12  env reap --provider sprites --json (dry-run): 0 recorded orphans, others
      listed as unrecorded
Each step prints PASS/FAIL; the first FAIL tears down and exits non-zero.
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
    local names n out
    names="$(run_records sprites)"
    echo "-- teardown: env down"
    (cd "$REPO" && "$THEGN_BIN" env down --force)
    for n in $names; do
      case "$n" in *[!a-z0-9-]*) continue ;; esac
      echo "-- teardown: env deprovision $n"
      "$THEGN_BIN" env deprovision "$n" --env "$env_name" # destroy proves ownership via this run's ledger
    done
    echo "-- teardown: env reap"
    "$THEGN_BIN" env reap --provider sprites --apply --yes
    out="$("$THEGN_BIN" fleet list --env "$env_name" --json 2>&1)"
    for n in $names; do
      if [ -f "$(ledger_dir)/$n.json" ] || printf '%s' "$out" | grep -Fq "\"$n\""; then
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
  else
    echo "FAIL $id: $desc" >&2
    exit 1
  fi
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
  local out
  # shellcheck disable=SC2016 # fixed text; script(1)'s shell expands the env values
  out="$(LIVE_BIN="$THEGN_BIN" LIVE_SPRITE="$SPRITE_NAME" SHELL=/bin/sh \
    script -qec 'exec "$LIVE_BIN" sprite-exec "$LIVE_SPRITE" -- sh -c "tty && echo ok"' /dev/null </dev/null)" || return 1
  printf '%s\n' "$out"
  printf '%s' "$out" | grep -Eq '(^|[[:space:]])ok([[:space:]]|$)'
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

pull_refs_present() { [ -n "$(git -C "$REPO" for-each-ref refs/thegn/pull)" ]; }

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
  out="$("$THEGN_BIN" fleet cost --json 2>&1)" || return 1
  printf '%s' "$out" | grep -q '"sprites_usage"' || return 1
  printf '%s' "$out" | grep -Eo '"cpu_seconds": *[0-9.eE+-]+' | sed 's/.*: *//' |
    awk 'BEGIN { ok = 1 } $1 + 0 > 0 { ok = 0 } END { exit ok }'
}

record_gone() { [ ! -f "$(ledger_dir)/$SPRITE_NAME.json" ]; }

reap_dry_clean() { # dry-run only: no recorded orphan, everything else unrecorded
  local out
  out="$("$THEGN_BIN" env reap --provider sprites --json 2>&1)" || return 1
  printf '%s\n' "$out" | head -40
  printf '%s' "$out" | grep -Fq "\"$SPRITE_NAME\"" && return 1
  printf '%s' "$out" | grep -Eq ': *"recorded"' && return 1
  return 0
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
step 6 "interactive exec has a tty" interactive_tty
step 6b "curl exists in the sprite (keep-awake precondition)" curl_present
step 7a "background hold runs (sprite-exec --hold 10m -- sleep 420)" hold_start
step 7b "/v1/tasks shows thegn-hold- during the hold" hold_task_present
step 7c "the hold exits clean" hold_finish
step 7d "/v1/tasks shows no thegn-hold- after the hold" hold_task_absent
step 7e "the hold had keep-awake (no 'keep-awake unavailable' on stderr)" hold_was_awake
step 8 "env pull" in_repo "$THEGN_BIN" env pull
step 8b "refs/thegn/pull is non-empty" pull_refs_present
step 9 "after 120 s idle the sprite is warm or cold" idle_warm_or_cold
step 9b "/v1/tasks is empty" tasks_empty
step 10 "fleet cost reports sprites_usage CPU seconds > 0" cost_has_cpu
step 10b "fleet list still shows warm or cold after fleet cost" warm_or_cold
step 11 "env down destroys the sprite" in_repo "$THEGN_BIN" env down
step 11b "this run's ledger record is gone" record_gone
step 12 "reap dry-run: 0 recorded orphans" reap_dry_clean
echo "ALL STEPS PASS"
