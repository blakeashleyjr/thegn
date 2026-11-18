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
# Usage:
#   test/sprites-live.sh                       # print plan, no network
#   THEGN_CHANNEL=dev test/sprites-live.sh --env <name> --i-accept-spend
set -euo pipefail

THEGN_BIN="${THEGN_BIN:-thegn}"

env_name="" accept=0
while [ $# -gt 0 ]; do
  case "$1" in
  --env)
    env_name="${2:?--env needs a value}"
    shift 2
    ;;
  --i-accept-spend)
    accept=1
    shift
    ;;
  -h | --help)
    sed -n 2,13p "$0"
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
Max spend   : one sprite for the length of this run (minutes); teardown destroys
              it. A crash leaves it until \`env reap --provider sprites\`.
Teardown    : trap on EXIT/INT/TERM -> env down --force, env reap --provider
              sprites --apply --yes, then fail if any sprite named in this run's
              ledger still exists.
Steps:
  1   scratch repo + env test          2   ledger has no record before create
  3   provision (env up): ledger creating -> ready
  4   run a command in the sprite (sprite-exec)
  4b  headless hold: sprite-exec --hold 5m registers/deletes thegn-hold-*
  5   env pull (fast-forward)          6   env down (destroy)
  7   env reap (dry-run, then apply): unrecorded sprites listed, never deleted
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

print_plan
echo
echo "== About to create: 1 sprite via env '$env_name' =="
echo "== temp state: $TMP =="

fleet_json() { "$THEGN_BIN" fleet list --json 2>&1; }

teardown() {
  local rc=$?
  trap - EXIT INT TERM
  set +e
  local left=0
  if [ "$CREATED" = 1 ]; then
    echo "-- teardown: env down"
    (cd "$REPO" && "$THEGN_BIN" env down --force)
    echo "-- teardown: env reap"
    "$THEGN_BIN" env reap --provider sprites --apply --yes
    echo "-- teardown: fleet list (no sprite from this run may remain)"
    local out
    out="$(fleet_json)"
    echo "$out" | head -40
    if [ "$SPRITE_NAME" != "(unknown)" ] && echo "$out" | grep -Fq "\"$SPRITE_NAME\""; then
      left=1
      echo "TEARDOWN INCOMPLETE: sprite $SPRITE_NAME may still exist; destroy it manually" >&2
      rc=1
    fi
  fi
  if [ "$left" = 1 ]; then
    echo "-- keeping $TMP (ledger needed for a later reap); ledger files:" >&2
    find "$TMP" -type f \( -path '*registry*' -o -name '*ledger*' \) -print -exec cat {} \; >&2 2>/dev/null
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

scratch_repo() {
  git -C "$REPO" init -q -b main &&
    git -C "$REPO" -c user.name=sx -c user.email=sx@example.invalid -c commit.gpgsign=false \
      commit -q --allow-empty -m init &&
    git -C "$REPO" switch -q -c sx/live-lane
}

sprite_name_from_fleet() {
  SPRITE_NAME="$(fleet_json 2>/dev/null |
    grep -Eo '"(sandbox_name|name)": *"[^"]+"' | head -1 | sed 's/.*: *"//; s/"$//')" || true
  [ -n "$SPRITE_NAME" ] || SPRITE_NAME="(unknown)"
}

step 1 "scratch repo + env test (token resolves)" bash -c "true"
scratch_repo
step 1b "env test" "$THEGN_BIN" env test "$env_name"
step 2 "ledger starts empty (fresh state: no rows)" bash -c "! '$THEGN_BIN' fleet list --json 2>&1 | grep -Eq '\"id\": *\"'"
in_repo "$THEGN_BIN" env set "$env_name" >/dev/null
CREATED=1
step 3 "provision sprite (env up)" in_repo "$THEGN_BIN" env up
sprite_name_from_fleet
echo "sprite: $SPRITE_NAME"
step 4 "command runs in the sprite" bash -c "'$THEGN_BIN' sprite-exec '$SPRITE_NAME' -- true </dev/null"
step 4b "headless hold wrapper runs and exits clean" bash -c "'$THEGN_BIN' sprite-exec --hold 5m '$SPRITE_NAME' -- true </dev/null"
step 5 "env pull fast-forwards the lane branch" in_repo "$THEGN_BIN" env pull
step 6 "env down destroys the sprite" in_repo "$THEGN_BIN" env down
step 7 "reap: dry-run then apply" bash -c "'$THEGN_BIN' env reap --provider sprites && '$THEGN_BIN' env reap --provider sprites --apply --yes"
echo "ALL STEPS PASS"
