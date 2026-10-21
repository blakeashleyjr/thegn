#!/usr/bin/env bash
# Stop and remove the root-owned ephemeral runner loops installed by setup.
set -euo pipefail

RUNNER_USER="${RUNNER_USER:-thegn-ci}"
RUNNER_BASE="${RUNNER_BASE:-/var/lib/thegn-ci/runners}"
RUNNER_TOKEN_FILE="${RUNNER_TOKEN_FILE:-/etc/thegn-ci/registration.token}"
RUNNER_LOOP_FILE="${RUNNER_LOOP_FILE:-/usr/local/libexec/thegn-ci/runner-loop}"
RUNNER_UNIT_FILE="${RUNNER_UNIT_FILE:-/etc/systemd/system/thegn-ci-runner@.service}"
REMOVE_USER="${REMOVE_USER:-0}"

log() { printf '\033[1;36m▸ %s\033[0m\n' "$*"; }
die() {
  printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2
  exit 1
}

main() {
  local active_units
  if [ "$REMOVE_USER" = 1 ] && [ -e /etc/NIXOS ]; then
    die 'NixOS runner users are declared in configuration; remove users.users.thegn-ci from configuration.nix and rebuild instead'
  fi
  if ! sudo systemctl stop 'thegn-ci-job-*'; then
    log 'no matching transient jobs to stop (or stop returned nonzero); checking active jobs before cleanup'
  fi
  active_units="$(sudo systemctl list-units 'thegn-ci-job-*' --state=active --no-legend --no-pager)" || die 'cannot verify transient jobs stopped; leaving runner files in place'
  [ -z "$active_units" ] || die "transient jobs are still active; leaving runner files in place:\n$active_units"
  sudo systemctl disable --now 'thegn-ci-runner@*.service' || true # best-effort: partial installs and no matching units are normal
  active_units="$(sudo systemctl list-units 'thegn-ci-runner@*.service' --state=active --no-legend --no-pager)" || die 'cannot verify runner loops stopped; leaving runner files in place'
  [ -z "$active_units" ] || die "runner loops are still active; leaving runner files in place:\n$active_units"
  sudo rm -f "$RUNNER_UNIT_FILE" "$RUNNER_LOOP_FILE"
  sudo rm -rf "$(dirname "$RUNNER_TOKEN_FILE")"
  sudo rm -rf "$RUNNER_BASE"
  sudo systemctl daemon-reload
  if [ "$REMOVE_USER" = 1 ]; then
    sudo userdel --remove "$RUNNER_USER"
  fi
  log 'ephemeral runner loops removed'
}

main "$@"
