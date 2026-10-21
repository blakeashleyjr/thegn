#!/usr/bin/env bash
# Stop and remove the root-owned ephemeral runner loops installed by setup.
set -euo pipefail

RUNNER_USER="${RUNNER_USER:-thegn-ci}"
RUNNER_COUNT="${RUNNER_COUNT:-4}"
RUNNER_BASE="${RUNNER_BASE:-/var/lib/thegn-ci/runners}"
RUNNER_LOOP_FILE="${RUNNER_LOOP_FILE:-/usr/local/libexec/thegn-ci/runner-loop}"
RUNNER_UNIT_FILE="${RUNNER_UNIT_FILE:-/etc/systemd/system/thegn-ci-runner@.service}"
REMOVE_USER="${REMOVE_USER:-0}"

log() { printf '\033[1;36m▸ %s\033[0m\n' "$*"; }
die() {
  printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2
  exit 1
}

main() {
  local index
  if [ "$REMOVE_USER" = 1 ] && [ -e /etc/NIXOS ]; then
    die 'NixOS runner users are declared in configuration; remove users.users.thegn-ci from configuration.nix and rebuild instead'
  fi
  for ((index = 1; index <= RUNNER_COUNT; index++)); do
    sudo systemctl disable --now "thegn-ci-runner@$index.service"
  done
  sudo rm -f "$RUNNER_UNIT_FILE" "$RUNNER_LOOP_FILE"
  sudo rm -rf /etc/thegn-ci
  sudo rm -rf "$RUNNER_BASE"
  sudo systemctl daemon-reload
  if [ "$REMOVE_USER" = 1 ]; then
    sudo userdel --remove "$RUNNER_USER"
  fi
  log 'ephemeral runner loops removed'
}

main "$@"
