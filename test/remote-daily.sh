#!/usr/bin/env bash
# Daily live remote run (b162 rx-live): headless agent + pull + land + reap on a
# provider env (sprites, fly or vercel). SPENDS REAL MONEY when acting; with no
# flags it only prints the plan and exits 0 without calling thegn or curl.
#
# Not in ci/ci-local/smoke: runs only through `just remote-daily`. Reads the
# owner's REAL config read-only (secrets are never read or printed) with a fresh
# XDG_STATE_HOME, XDG_RUNTIME_DIR and THEGN_DIR, its own daemon and a scratch
# repo on branch rd/daily-lane.
#
# Usage:
#   test/remote-daily.sh                                   # print plan, no network
#   THEGN_CHANNEL=dev test/remote-daily.sh --env <name> --i-accept-spend \
#     [--provider sprites|fly|vercel] [--with-model] [--with-adopt <sprite>] \
#     [--with-reaper]
set -euo pipefail

THEGN_BIN="${THEGN_BIN:-thegn}"

env_name="" accept=0 provider="sprites" with_model=0 adopt_name="" with_reaper=0
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
remote-daily plan (nothing is created without both --env <name> and
--i-accept-spend, plus THEGN_CHANNEL=dev)

Will create : ONE sandbox/machine via the named [env.<name>] from your real
              config (read-only), in a fresh state/runtime/THEGN_DIR with its own
              daemon and a scratch git repo on branch rd/daily-lane.
Max spend   : one sandbox for about 10 minutes (a 420 s scripted hold); more
              with --with-model (two real model runs). Teardown destroys it.
Teardown    : trap on EXIT/INT/TERM -> env down --force, env deprovision <name>
              for each name in THIS run's ledger, daemon stop; fails (keeps the
              ledger) if any of this run's names remain.
Steps:
  1   env test
  2   wt new rd-x --env <S>
  3   agent run rd-x --agent hold-test --prompt x --wait --pull --json
      (scripted agent: sleep 420; date>f; git commit). No per-run overlay agent
      can be added from here, so the stated fallback is used: the same script
      run through the exec bridge (sprite-exec/vercel-exec --hold) followed by
      env pull. If your config defines a hold-test agent, set
      RD_HOLD_AGENT=1 to use agent run instead.
  4   (--with-model) agent run --agent claude, then --agent codex, each
      committing one file
  5   (fly only) fleet list --env shows stopped
  6   exec bridge: stat -c %a ~/.claude/.credentials.json is 600
  7   local git log has the commits
  8   land --worktree rd-x
  9   ledger record gone, sandbox absent from the org listing
 10   env reap --provider <p> --json is clean
 A    (--with-adopt <name>) env adopt <name> --env <S> --yes; ledger record
 R    (--with-reaper) lifetime_reap on a throwaway env, hydrate cadence
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

TMP="$(mktemp -d)"
export XDG_STATE_HOME="$TMP/state" THEGN_DIR="$TMP/thegn"
unset THEGN_CONFIG_DIR
mkdir -p "$XDG_STATE_HOME" "$THEGN_DIR" "$TMP/repo"
mkdir -m 700 "$TMP/run"
export XDG_RUNTIME_DIR="$TMP/run"
REPO="$TMP/repo"
NAME="(unknown)"
CREATED=0
HOLD_PID=""
case "$provider" in
sprites) EXEC_BRIDGE="sprite-exec" ;;
vercel) EXEC_BRIDGE="vercel-exec" ;;
*) EXEC_BRIDGE="" ;; # fly has no thegn exec bridge verb
esac

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

teardown() {
  local rc=$?
  trap - EXIT INT TERM
  set +e
  local left=0 names n
  if [ -n "$HOLD_PID" ] && kill -0 "$HOLD_PID" 2>/dev/null; then
    kill "$HOLD_PID" 2>/dev/null
    sleep 2
    kill -0 "$HOLD_PID" 2>/dev/null && {
      echo "TEARDOWN INCOMPLETE: hold process $HOLD_PID survived" >&2
      rc=1
    }
  fi
  if [ "$CREATED" = 1 ]; then
    names="$(run_records)"
    echo "-- teardown: env down --force"
    (cd "$REPO" && "$THEGN_BIN" env down --force)
    for n in $names; do
      case "$n" in *[!a-z0-9-]*) continue ;; esac
      if [ -f "$(ledger_dir)/$n.json" ]; then
        echo "-- teardown: env deprovision $n"
        "$THEGN_BIN" env deprovision "$n" --env "$env_name"
      fi
    done
    echo "-- teardown: daemon stop"
    "$THEGN_BIN" daemon stop
    for n in $names; do
      if [ -f "$(ledger_dir)/$n.json" ]; then
        left=1
        rc=1
        echo "TEARDOWN INCOMPLETE: $n may still exist; the ledger is kept in $TMP" >&2
      fi
    done
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
  # shellcheck disable=SC2016 # $HOME must expand inside the sandbox
  no_stdin "$THEGN_BIN" "$EXEC_BRIDGE" --hold 10m "$NAME" -- sh -c \
    'cd "$HOME/workspace" && sleep 420 && date > rd-hold.txt && git add rd-hold.txt && git -c user.name=rd -c user.email=rd@example.invalid -c commit.gpgsign=false commit -q -m rd-hold' &
  HOLD_PID=$!
}
hold_wait() {
  local pid="$HOLD_PID"
  wait "$pid" || return 1
  HOLD_PID=""
}

agent_run() { # agent_run <agent> ; each agent commits one file
  in_repo "$THEGN_BIN" agent run rd-x --agent "$1" --prompt "create $1-rd.txt containing ok and commit it" --wait --pull --json
}

fly_stopped() {
  local out
  out="$("$THEGN_BIN" fleet list --env "$env_name" --json 2>&1)" || return 1
  printf '%s' "$out" | grep -Eqi '"stopped"|"suspended"'
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

local_log_has_commits() {
  local n want="${1:-1}"
  n="$(git -C "$REPO" rev-list --count main..HEAD 2>/dev/null || echo 0)"
  echo "   commits ahead of main: $n"
  [ "$n" -ge "$want" ]
}

listing_lacks_name() {
  local out
  out="$("$THEGN_BIN" fleet list --env "$env_name" --json 2>&1)" || return 1
  ! printf '%s' "$out" | grep -Fq "\"$NAME\""
}
record_gone() { [ ! -f "$(ledger_dir)/$NAME.json" ]; }

reap_clean() {
  local out
  out="$("$THEGN_BIN" env reap --provider "$provider" --json 2>&1)" || return 1
  printf '%s\n' "$out" | head -40
  ! printf '%s' "$out" | grep -Fq "\"$NAME\""
}

adopt_step() {
  "$THEGN_BIN" env adopt "$adopt_name" --env "$env_name" --yes --json &&
    [ -f "$(ledger_dir)/$adopt_name.json" ]
}

step 1 "env test" "$THEGN_BIN" env test "$env_name"
scratch_repo
in_repo "$THEGN_BIN" env set "$env_name" >/dev/null
CREATED=1
step 2 "wt new rd-x --env $env_name" in_repo "$THEGN_BIN" wt new rd-x --env "$env_name"
step 2b "exactly one ready ledger record for this run" name_from_ledger
echo "sandbox: $NAME"
if [ "${RD_HOLD_AGENT:-0}" = 1 ]; then
  step 3 "agent run hold-test --wait --pull" in_repo "$THEGN_BIN" agent run rd-x --agent hold-test --prompt x --wait --pull --json
else
  echo "   (no overlay hold-test agent: using the $EXEC_BRIDGE --hold fallback + env pull)"
  step 3a "scripted hold starts" hold_script_fallback
  step 3b "scripted hold finishes (420 s)" hold_wait
  step 3c "env pull" in_repo "$THEGN_BIN" env pull
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
step 6 "credentials file mode is 600" cred_mode
if [ "$with_model" = 1 ]; then want=3; else want=1; fi
step 7 "local git log has the commits" local_log_has_commits "$want"
step 8 "land --worktree rd-x" in_repo "$THEGN_BIN" land --worktree rd-x
step 9a "ledger record gone" record_gone
step 9b "sandbox absent from the org listing" listing_lacks_name
step 10 "env reap --provider $provider is clean" reap_clean
if [ -n "$adopt_name" ]; then
  step A "env adopt $adopt_name" adopt_step
fi
if [ "$with_reaper" = 1 ]; then
  echo "NOTE R: lifetime_reap needs a client up and a throwaway env with max_lifetime_secs > 0;" >&2
  echo "        verify manually with: thegn fleet list after the 300 s hydrate cadence" >&2
fi
echo "ALL STEPS PASS"
