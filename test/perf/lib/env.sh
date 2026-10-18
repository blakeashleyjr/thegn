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
  local entry v real_runtime
  real_runtime="${XDG_RUNTIME_DIR:-}"
  # A perf run must not inherit the developer's session controls, log routing,
  # or credentials. Keep TG_PERF_* so the harness's own knobs remain available.
  while IFS= read -r -d '' entry; do
    v="${entry%%=*}"
    case "$v" in
    THEGN_* | GH_* | GITHUB_* | KANEO_* | LINEAR_* | JIRA_* | AWS_* | *TOKEN* | *API_KEY* | *SECRET*)
      case "$v" in
      TG_PERF_*) ;;
      *) unset "$v" ;;
      esac
      ;;
    esac
  done < <(env -0)

  # A sandbox-on perf run may use the host's user manager, but the harness must
  # keep its daemon socket private under the isolated runtime dir below.
  unset DBUS_SESSION_BUS_ADDRESS
  if [ -n "$real_runtime" ] && [ -S "$real_runtime/bus" ]; then
    export DBUS_SESSION_BUS_ADDRESS="unix:path=$real_runtime/bus"
  fi
  if [ "${TG_PERF_SANDBOX:-}" = 1 ] && [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    echo 'perf: TG_PERF_SANDBOX=1 requires a user bus at the original XDG_RUNTIME_DIR/bus socket' >&2
    return 1
  fi

  PERF_TMP="$(mktemp -d "${TMPDIR:-/tmp}/tg-perf.XXXXXX")"
  export PERF_TMP
  export HOME="$PERF_TMP/home"
  export XDG_CONFIG_HOME="$PERF_TMP/config"
  export XDG_STATE_HOME="$PERF_TMP/state"
  export XDG_CACHE_HOME="$PERF_TMP/cache"
  # ALSO the runtime dir, and this one is not cosmetic. The pane daemon's
  # control socket resolves to
  # `$XDG_RUNTIME_DIR/thegn/<scope-key>/daemon.sock` FIRST and
  # only falls back to the isolated `$XDG_STATE_HOME/thegn/run/` when that is
  # unset. Leaving the developer's real `/run/user/<uid>` in place means a
  # daemon-enabled scenario binds — or worse, attaches to — the daemon owning
  # their live session's panes. Harmless while every scenario passed
  # THEGN_NO_DAEMON=1; a trap the moment one does not (see `soak-daemon`).
  export XDG_RUNTIME_DIR="$PERF_TMP/run"
  export GIT_CONFIG_GLOBAL="$PERF_TMP/gitconfig"
  export GIT_CONFIG_SYSTEM=/dev/null
  # thegn spawns pane shells from `$SHELL` verbatim (panes.rs pane_shell_argv).
  # Point it at a uniquely named symlink to the real bash so the spawn-rate
  # parser can recognise ONLY the harness-owned pane shell by exact path
  # (PERF_PANE_SHELL) instead of guessing from argv shape.
  PERF_PANE_SHELL="$PERF_TMP/bin/thegn-perf-pane-shell"
  mkdir -p "$PERF_TMP/bin"
  ln -s "$(command -v bash)" "$PERF_PANE_SHELL"
  export PERF_PANE_SHELL
  export SHELL="$PERF_PANE_SHELL"
  mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"
  # 0700 to match the real run dir: the daemon rejects a lax one.
  mkdir -p "$XDG_RUNTIME_DIR"
  chmod 700 "$XDG_RUNTIME_DIR"
  # ~/.gitconfig is masked as a *directory* in some sandboxes; GIT_CONFIG_GLOBAL
  # points git at this file unconditionally.
  printf '[user]\n\tname = perf\n\temail = perf@example.invalid\n[init]\n\tdefaultBranch = main\n' \
    >"$GIT_CONFIG_GLOBAL"
}

# Track background launch process groups so EXIT cleanup can reap descendants
# even when timeout(1) only reports that its direct child timed out.
PERF_PROCESS_GROUPS=()
perf_track_process_group() {
  PERF_PROCESS_GROUPS+=("$1")
}

perf_sandbox_off_default() {
  if [ "${TG_PERF_SANDBOX:-}" != 1 ]; then
    mkdir -p "$XDG_CONFIG_HOME/thegn"
    printf '\n[sandbox]\nenabled = false\n' >>"$XDG_CONFIG_HOME/thegn/config.toml"
  fi
}

perf_stop_process_group() {
  local pgid="$1"
  kill -TERM -- "-$pgid" 2>/dev/null || true
  sleep 1
  kill -KILL -- "-$pgid" 2>/dev/null || true
  wait "$pgid" 2>/dev/null || true
}

perf_watch_process_group() {
  local pgid="$1" deadline_s="$2"
  (
    for ((tick = 0; tick < deadline_s * 10; tick++)); do
      kill -0 "$pgid" 2>/dev/null || exit 0
      sleep 0.1
    done
    if kill -0 "$pgid" 2>/dev/null; then
      kill -TERM -- "-$pgid" 2>/dev/null || true
      sleep 1
      kill -KILL -- "-$pgid" 2>/dev/null || true
    fi
  ) &
  perf_track_process_group "$!"
}

# Remove any podman pause process owned by this fixture, then remove its files.
# Podman overlay layers may be owned by its user namespace, so retry deletion
# through `podman unshare` when ordinary rm cannot remove the isolated tree.
perf_cleanup() {
  local d cmdline pgid
  if [ "${#PERF_PROCESS_GROUPS[@]}" -gt 0 ]; then
    for pgid in "${PERF_PROCESS_GROUPS[@]}"; do
      kill -TERM -- "-$pgid" 2>/dev/null || true
    done
    sleep 1
    for pgid in "${PERF_PROCESS_GROUPS[@]}"; do
      kill -KILL -- "-$pgid" 2>/dev/null || true
      wait "$pgid" 2>/dev/null || true
    done
  fi
  if [ -n "${PERF_SOCK_DIR:-}" ]; then
    rm -rf -- "$PERF_SOCK_DIR" || true
  fi
  if [ -n "${PERF_TMP:-}" ] && [ -e "$PERF_TMP" ]; then
    if ! rm -rf -- "$PERF_TMP" && command -v podman >/dev/null 2>&1; then
      podman unshare rm -rf -- "$PERF_TMP" || true
    fi
  fi
  # `podman unshare` can start a new pause process with this fixture's runtime
  # environment, so scan after both deletion attempts. Match a path component.
  if [ -n "${PERF_TMP:-}" ] && [ -d /proc ]; then
    for d in /proc/[0-9]*; do
      [ -r "$d/cmdline" ] && [ -r "$d/environ" ] || continue
      cmdline="$(tr '\0' ' ' <"$d/cmdline" 2>/dev/null || true)"
      case "$cmdline" in
      *podman\ pause*)
        if tr '\0' '\n' <"$d/environ" 2>/dev/null | grep -F -- "$PERF_TMP/" >/dev/null; then
          kill "${d##*/}" 2>/dev/null || true
        fi
        ;;
      esac
    done
  fi
}

# Install an EXIT trap that removes the temp dir.
perf_trap_cleanup() {
  trap perf_cleanup EXIT
}

# A stable per-machine tag so baselines are explicitly machine-scoped.
# uname-arch + a short hash of the CPU model line.
#
# The model line has to come from somewhere on every platform, or the tag
# degrades to `<arch>-<hash of "unknown">` and EVERY machine of that arch shares
# one baseline — which is worse than having none, because a regression on one
# box silently rewrites the reference for another. macOS has no /proc/cpuinfo;
# `machdep.cpu.brand_string` is its equivalent.
perf_host_tag() {
  local arch model hash
  arch="$(uname -m)"
  if [ -r /proc/cpuinfo ]; then
    model="$(awk -F': ' '/model name/{print $2; exit}' /proc/cpuinfo 2>/dev/null || echo unknown)"
  else
    model="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)"
  fi
  hash="$(printf '%s' "$model" | cksum | cut -d' ' -f1)"
  printf '%s-%s' "$arch" "$hash"
}
