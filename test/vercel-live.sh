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
#     --team-id team_x --project-id prj_x --i-accept-spend [--with-model-login]
set -euo pipefail

THEGN_BIN="${THEGN_BIN:-thegn}"
ENV_NAME="vxlive"
MAX_LIFETIME=3600
MAX_INSTANCES=1
VCPUS=2

token_ref="" team_id="" project_id="" accept=0 model_login=0
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

print_plan() {
  cat <<PLAN
vercel-live plan (nothing is created without all of: --token-ref, --team-id,
--project-id, --i-accept-spend and THEGN_CHANNEL=dev)

Will create : ONE Vercel sandbox, ${VCPUS} vCPU, max_lifetime_secs = ${MAX_LIFETIME},
              max_instances = ${MAX_INSTANCES}, in a fresh temp XDG state/config dir
              and a scratch git repo. The token value is never read or printed.
Max spend   : bounded by ${VCPUS} vCPU x 1 h of active CPU plus memory and snapshot
              storage; roughly under US\$1 at Pro iad1 rates (estimate).
Teardown    : trap on EXIT/INT/TERM -> env down, env reap --provider vercel
              --apply --yes (temp state), then fleet list must show 0 rows.
Steps:
  1   create env config (temp)        2   env test (token works)
  3   provision sandbox (env up)      4   run a command in the sandbox
  5   interactive terminal open       5b  record the interactive host
  6   commit in sandbox               6b  in-sandbox curl must fail (egress)
  6b-m..6e upload mode (only --with-model-login)
  3b  exactly one vercel ledger record for this run
  7   env pull (fast-forward)         8   stop (env down --stop), 8a resume
  8b  tmpfs login dir is gone after stop/resume
  9   env down (destroy)              10  env reap dry-run, 10a apply
Each step prints PASS/FAIL; the first FAIL tears down and exits non-zero.
PLAN
}

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

TMP="$(mktemp -d)"
export XDG_STATE_HOME="$TMP/state" XDG_CONFIG_HOME="$TMP/config" THEGN_DIR="$TMP/thegn"
unset THEGN_CONFIG_DIR
mkdir -p "$XDG_STATE_HOME" "$XDG_CONFIG_HOME/thegn" "$THEGN_DIR" "$TMP/repo"
mkdir -m 700 "$TMP/run"
export XDG_RUNTIME_DIR="$TMP/run"
REPO="$TMP/repo"
SANDBOX_NAME="(unknown)"
CREATED=0

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

teardown() {
  local rc=$?
  trap - EXIT INT TERM
  set +e
  local left=0
  if [ "$CREATED" = 1 ]; then
    local names n
    names="$(run_records vercel)"
    echo "-- teardown: env down"
    (cd "$REPO" && "$THEGN_BIN" env down --force)
    echo "-- teardown: env reap"
    "$THEGN_BIN" env reap --provider vercel --apply --yes
    echo "-- teardown: fleet list (must show 0 rows)"
    local out
    out="$("$THEGN_BIN" fleet list --json 2>&1)"
    echo "$out" | head -40
    if echo "$out" | grep -Eq '"id": *"'; then
      left=1
      echo "TEARDOWN INCOMPLETE: sandbox may still exist; clean up manually: $SANDBOX_NAME (team $team_id, project $project_id)" >&2
      rc=1
    fi
    for n in $names; do
      if [ -f "$(ledger_dir)/$n.json" ]; then
        left=1
        rc=1
        echo "TEARDOWN INCOMPLETE: sandbox $n may still exist; the ledger is kept in $TMP" >&2
      fi
    done
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
  case "$out" in *curl_rc=0* | *curl_rc=127*) return 1 ;; *curl_rc=*) return 0 ;; *) return 1 ;; esac
}

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
  fi
}

scratch_repo() {
  git -C "$REPO" init -q -b main &&
    git -C "$REPO" -c user.name=vx -c user.email=vx@example.invalid -c commit.gpgsign=false \
      commit -q --allow-empty -m init &&
    git -C "$REPO" switch -q -c vx/live-lane
}

step 1 "write temp config + scratch repo" bash -c "true"
write_config
scratch_repo
step 2 "env test (token resolves, list() works)" "$THEGN_BIN" env test "$ENV_NAME"
in_repo "$THEGN_BIN" env set "$ENV_NAME" >/dev/null
CREATED=1
step 3 "provision sandbox (env up)" in_repo "$THEGN_BIN" env up
step 3b "exactly one vercel ledger record for this run" name_from_ledger
echo "sandbox: $SANDBOX_NAME"
step 4 "command runs in the sandbox (vercel-exec)" no_stdin "$THEGN_BIN" vercel-exec "$SANDBOX_NAME" -- true
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
step 8 "stop (env down --stop)" in_repo "$THEGN_BIN" env down --stop
step 8a "resume (env up)" in_repo "$THEGN_BIN" env up
step 8b "tmpfs login dir is gone after stop/resume" ask "/dev/shm/thegn-login-* absent after resume? "
step 9 "env down destroys the sandbox" in_repo "$THEGN_BIN" env down
step 10 "reap: dry-run" "$THEGN_BIN" env reap --provider vercel
step 10a "reap: apply finds nothing left" "$THEGN_BIN" env reap --provider vercel --apply --yes
echo "ALL STEPS PASS"
