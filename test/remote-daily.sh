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
#     [--with-reaper] [--keep-going]
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
  --keep-going)
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
Options     : --keep-going is accepted for plan-only runs; it does not bypass
              --i-accept-spend.
Teardown    : trap on EXIT/INT/TERM -> env down --force, env deprovision <name>
              for each name in THIS run's ledger (rd-x's and rd-y's), daemon
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
      (scripted agent: sleep 420; date>f; git commit). No per-run overlay agent
      can be added from here, so the stated fallback is used: the same script
      run through the exec bridge (sprite-exec --hold / vercel-exec) followed
      by env pull. If your config defines a hold-test agent, set
      RD_HOLD_AGENT=1 to use agent run instead (required for fly, which has
      no exec bridge).
  4   (--with-model) agent run --agent claude, then --agent codex, each
      committing one file
  5   (fly only) fleet list --env shows stopped
  6   if login_upload=claude, exec bridge stat -c %a ~/.claude/.credentials.json is 600;
      otherwise no model login entered the sandbox (model_credentials default)
  7   local git log has the commits
  8   land --worktree <rd-x path> --release-sandbox
  9   ledger record gone, sandbox absent from the org listing
 10   env reap --provider <p> --json is clean
 11   wt new rd-y --env <S>
11p   resolve the rd-y worktree path
11u   env up (provision rd-y)
11b   exactly one new ledger record
      then wt rm <rd-y path> --force exits 0 (the worktree-delete teardown,
      which confirms the destroy)     11c its record is gone and the org
      listing lacks it
 A    (--with-adopt <name>) env adopt <name> --env <S> --yes; ledger record
      is checked, then that record file is removed: the adopted sprite was
      not created by this run and is NEVER destroyed by this script
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

if [ "$provider" = fly ] && [ "${RD_HOLD_AGENT:-0}" != 1 ]; then
  echo "fly needs RD_HOLD_AGENT=1 (no exec bridge)" >&2
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
with open(os.path.join(os.environ["TMP"], "preflight.env"), "w", encoding="utf-8") as f:
    f.write("LOGIN_UPLOAD=" + show(row.get("login_upload")) + "\n")
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
  printf '%s' "$DOCTOR_JSON" | PRE_ENV="$1" PRE_REQUIRED="${2:-1}" TMP="$TMP" python3 -I -c "$PREFLIGHT_PY" || {
    echo "FAIL 0: preflight" >&2
    exit 1
  }
}
# Run against the owner's real state, before the fresh dirs below, so
# pre-existing unbound sandboxes are listed.
preflight_capture

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
WT_PATH=""
RDY_NAME=""
RDY_PATH=""
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
  if [ "$CREATED" = 1 ]; then
    # rd-y's name came from this run's ledger (11b); keep it even if its
    # record is gone, so the listing check below still covers it.
    names="$(run_records)"
    if [ -n "$RDY_NAME" ] && ! printf '%s\n' "$names" | grep -Fxq "$RDY_NAME"; then
      names="$names $RDY_NAME"
    fi
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

agent_run() { # agent_run <agent> ; each agent commits one file
  in_repo "$THEGN_BIN" agent run --worktree "$WT_PATH" --agent "$1" --prompt "create $1-rd.txt containing ok and commit it" --wait --pull --json
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

no_model_login() {
  [ -n "$EXEC_BRIDGE" ] || return 0
  local out
  # shellcheck disable=SC2016 # $HOME must expand inside the sandbox
  out="$(no_stdin "$THEGN_BIN" "$EXEC_BRIDGE" "$NAME" -- sh -c 'test ! -e "$HOME/.claude/.credentials.json" && echo absent')" || return 1
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
  local n new=""
  for n in $(run_records); do
    printf '%s\n' "$RECORDS_BEFORE" | grep -Fxq "$n" || new="$new $n"
  done
  new="${new# }"
  echo "   new ledger records: ${new:-(none)}"
  [ -n "$new" ] || return 1
  case "$new" in *' '* | *[!a-z0-9-]*) return 1 ;; esac
  grep -Eq '"state": *"ready"' "$(ledger_dir)/$new.json" || return 1
  RDY_NAME="$new"
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
  step 3 "agent run hold-test --wait --pull" in_repo "$THEGN_BIN" agent run --worktree "$WT_PATH" --agent hold-test --prompt x --wait --pull --json
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
if [ "$LOGIN_UPLOAD" = claude ]; then
  step 6 "credentials file mode is 600" cred_mode
else
  step 6 "no model login entered the sandbox (model_credentials default)" no_model_login
fi
if [ "$with_model" = 1 ]; then want=3; else want=1; fi
step 7 "local git log has the commits" local_log_has_commits "$want"
step 8 "land --worktree <rd-x> --release-sandbox" in_repo "$THEGN_BIN" land --worktree "$WT_PATH" --release-sandbox
step 9a "ledger record gone" record_gone
step 9b "sandbox absent from the org listing" listing_lacks_name
step 10 "env reap --provider $provider is clean" reap_clean
snapshot_records
step 11 "wt new rd-y --env $env_name" in_repo "$THEGN_BIN" wt new rd-y --env "$env_name"
step 11p "resolve the rd-y worktree path" resolve_rdy_path
step 11u "env up (provision rd-y)" no_stdin "$THEGN_BIN" env up --worktree "$RDY_PATH"
step 11b "exactly one new ledger record for rd-y" one_new_record
echo "sandbox (rd-y): $RDY_NAME"
# --force only here: rd-y is this run's scratch worktree.
step 11r "wt rm <rd-y path> --force exits 0" no_stdin in_repo "$THEGN_BIN" wt rm "$RDY_PATH" --force
step 11c "rd-y's ledger record is gone and the org listing lacks it" rdy_gone
if [ -n "$adopt_name" ]; then
  step A "env adopt $adopt_name" adopt_step
fi
if [ "$with_reaper" = 1 ]; then
  echo "NOTE R: lifetime_reap needs a client up and a throwaway env with max_lifetime_secs > 0;" >&2
  echo "        verify manually with: thegn fleet list after the 300 s hydrate cadence" >&2
fi
echo "ALL STEPS PASS"
