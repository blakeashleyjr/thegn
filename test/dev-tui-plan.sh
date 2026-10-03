#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

start_plan="$(just --dry-run start-term dev 2>&1)"
[[ $start_plan == *'target/debug/thegn'* ]] || {
  echo "start-term should launch the native thegn binary" >&2
  echo "$start_plan" >&2
  exit 1
}
# Ownership, not raw text: the plan must go through the generation-checked
# lifecycle helper and never signal a pid read straight from the pid file.
[[ $start_plan == *'thegn.pid'* && $start_plan == *'inst_restart'* && $start_plan == *'test/lib/instance.sh'* ]] || {
  echo "start-term should stop the prior named thegn via the identity-checked helper" >&2
  echo "$start_plan" >&2
  exit 1
}
# shellcheck disable=SC2016 # literal match of the forbidden shell text
[[ $start_plan != *'kill "$(cat'* && $start_plan != *'pkill'* ]] || {
  echo "launch plans must not kill a raw pid-file integer or pkill -f" >&2
  exit 1
}
[[ $start_plan != *$'\nsetsid -f'* ]] || {
  echo "start-term should keep pidfile variables and setsid in the same shell" >&2
  echo "$start_plan" >&2
  exit 1
}
[[ $start_plan == *'exec env'* ]] || {
  echo "start-term should exec through a pidfile wrapper so the tracked pid is thegn" >&2
  echo "$start_plan" >&2
  exit 1
}
# (The pre-rename check that the plan does not launch the LEGACY zellij-wrapper
# binary is gone: the legacy binary's pre-rename name no longer exists as a
# distinct spelling after the thegn rename, so the check had become a literal
# contradiction of the native-binary assertion above.)
[[ $start_plan != *'THEGN_ZELLIJ_BIN'* ]] || {
  echo "start-term should not configure the zellij/WASM path" >&2
  echo "$start_plan" >&2
  exit 1
}

inline_plan="$(just --dry-run start term 2>&1)"
# shellcheck disable=SC2016 # literal match
[[ $inline_plan == *'thegn.pid'* && $inline_plan == *'inst_restart "$pidfile" $$'* && $inline_plan == *'exec env'* ]] || {
  echo "start name should restart the prior named thegn (identity-checked) before execing inline" >&2
  echo "$inline_plan" >&2
  exit 1
}
# shellcheck disable=SC2016 # literal match
[[ $start_plan == *'inst_record "$pidfile"'* ]] || {
  echo "start-term's ghostty child must record its generation via inst_record" >&2
  exit 1
}
for recipe in start-dev start-mq; do
  r_plan="$(just --dry-run "$recipe" dev 2>&1)"
  # shellcheck disable=SC2016 # literal match
  [[ $r_plan == *'inst_restart "$pidfile" $$'* && $r_plan != *'kill "$(cat'* && $r_plan != *pkill* ]] || {
    echo "$recipe must restart via the identity-checked helper" >&2
    exit 1
  }
done
release_plan="$(just --dry-run start-term-release dev 2>&1)"
# shellcheck disable=SC2016 # literal match
[[ $release_plan == *'inst_record "$pidfile"'* ]] || {
  echo "start-term-release's ghostty child must record its generation via inst_record" >&2
  exit 1
}
# shellcheck disable=SC2016 # literal match of the forbidden shell text
[[ $release_plan != *pkill* && $release_plan == *'inst_rotate_scoped "$state"'* ]] || {
  echo "start-term-release must rotate daemons scoped to its state root, not pkill -f" >&2
  exit 1
}

dev_plan="$(just --dry-run dev-tui dev 2>&1)"
[[ $dev_plan == *'just start-term dev'* ]] || {
  echo "dev-tui should relaunch via start-term" >&2
  echo "$dev_plan" >&2
  exit 1
}
[[ $dev_plan != *'-w plugin'* && $dev_plan != *'-w layouts'* && $dev_plan != *'-w config'* ]] || {
  echo "native dev-tui should not watch legacy plugin/layout/config paths" >&2
  echo "$dev_plan" >&2
  exit 1
}

echo "dev-tui plan checks passed"
