#!/usr/bin/env bash
# Shared hermetic-environment preamble for the perf harness.
#
# Mirrors the justfile `_e2e_env` block: redirect HOME, the XDG dirs, and git
# config into a throwaway temp dir so the harness can neither read the
# developer's real config/gitconfig nor leak state into the daily thegn DB.
# A memory note is explicit that zellij-era tests leaked into the daily DB
# unless XDG_STATE_HOME (not just HOME) was sandboxed — so isolate all of them.
#
# Source this, then call `perf_make_tmp` to populate $PERF_TMP and the env.

set -euo pipefail

# Create the throwaway root and export the isolated environment. Caller is
# responsible for cleanup (or use perf_trap_cleanup).
perf_make_tmp() {
  PERF_TMP="$(mktemp -d "${TMPDIR:-/tmp}/sz-perf.XXXXXX")"
  export PERF_TMP
  export HOME="$PERF_TMP/home"
  export XDG_CONFIG_HOME="$PERF_TMP/config"
  export XDG_STATE_HOME="$PERF_TMP/state"
  export XDG_CACHE_HOME="$PERF_TMP/cache"
  export GIT_CONFIG_GLOBAL="$PERF_TMP/gitconfig"
  export GIT_CONFIG_SYSTEM=/dev/null
  mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"
  # ~/.gitconfig is masked as a *directory* in some sandboxes; GIT_CONFIG_GLOBAL
  # points git at this file unconditionally.
  printf '[user]\n\tname = perf\n\temail = perf@example.invalid\n[init]\n\tdefaultBranch = main\n' \
    >"$GIT_CONFIG_GLOBAL"
}

# Install an EXIT trap that removes the temp dir.
perf_trap_cleanup() {
  trap 'rm -rf "${PERF_TMP:-}"' EXIT
}

# A stable per-machine tag so baselines are explicitly machine-scoped.
# uname-arch + a short hash of the CPU model line.
perf_host_tag() {
  local arch model hash
  arch="$(uname -m)"
  model="$(awk -F': ' '/model name/{print $2; exit}' /proc/cpuinfo 2>/dev/null || echo unknown)"
  hash="$(printf '%s' "$model" | cksum | cut -d' ' -f1)"
  printf '%s-%s' "$arch" "$hash"
}
