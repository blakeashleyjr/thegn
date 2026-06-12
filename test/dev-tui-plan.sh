#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

start_plan="$(just --dry-run start-term dev 2>&1)"
[[ $start_plan == *'target/debug/szhost'* ]] || {
  echo "start-term should launch the native szhost binary" >&2
  echo "$start_plan" >&2
  exit 1
}
# shellcheck disable=SC2016 # the single quotes match a LITERAL `$(cat …)` in the plan
[[ $start_plan == *'szhost.pid'* && $start_plan == *'kill "$(cat "$pidfile")"'* ]] || {
  echo "start-term should stop the prior named szhost before relaunching" >&2
  echo "$start_plan" >&2
  exit 1
}
[[ $start_plan != *$'\nsetsid -f'* ]] || {
  echo "start-term should keep pidfile variables and setsid in the same shell" >&2
  echo "$start_plan" >&2
  exit 1
}
[[ $start_plan == *'exec env'* ]] || {
  echo "start-term should exec through a pidfile wrapper so the tracked pid is szhost" >&2
  echo "$start_plan" >&2
  exit 1
}
[[ $start_plan != *'target/debug/superzej'* ]] || {
  echo "start-term should not launch the legacy zellij superzej binary" >&2
  echo "$start_plan" >&2
  exit 1
}
[[ $start_plan != *'SUPERZEJ_ZELLIJ_BIN'* ]] || {
  echo "start-term should not configure the zellij/WASM path" >&2
  echo "$start_plan" >&2
  exit 1
}

inline_plan="$(just --dry-run start term 2>&1)"
# shellcheck disable=SC2016 # the single quotes match a LITERAL `$(cat …)` in the plan
[[ $inline_plan == *'szhost.pid'* && $inline_plan == *'kill "$(cat "$pidfile")"'* && $inline_plan == *'exec env'* ]] || {
  echo "start name should also restart the prior named szhost before execing inline" >&2
  echo "$inline_plan" >&2
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
