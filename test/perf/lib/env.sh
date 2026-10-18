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

  # Only sandbox-on measurements need the host user manager. Expose its private
  # socket in the isolated runtime dir, keeping the session bus private.
  unset DBUS_SESSION_BUS_ADDRESS

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
  if [ "${TG_PERF_SANDBOX:-}" = 1 ]; then
    if [ -n "$real_runtime" ] && [ -S "$real_runtime/systemd/private" ]; then
      mkdir -p "$XDG_RUNTIME_DIR/systemd"
      ln -s "$real_runtime/systemd/private" "$XDG_RUNTIME_DIR/systemd/private"
    elif [ -n "$real_runtime" ] && [ -S "$real_runtime/bus" ]; then
      export DBUS_SESSION_BUS_ADDRESS="unix:path=$real_runtime/bus"
      echo 'perf: user manager private socket unavailable; exposing the session bus for TG_PERF_SANDBOX=1' >&2
    else
      echo 'perf: TG_PERF_SANDBOX=1 requires a user manager socket or session bus in the original XDG_RUNTIME_DIR' >&2
      return 1
    fi
  fi
  # ~/.gitconfig is masked as a *directory* in some sandboxes; GIT_CONFIG_GLOBAL
  # points git at this file unconditionally.
  printf '[user]\n\tname = perf\n\temail = perf@example.invalid\n[init]\n\tdefaultBranch = main\n' \
    >"$GIT_CONFIG_GLOBAL"
}

# Track background launch process groups so EXIT cleanup can reap descendants
# even when timeout(1) only reports that its direct child timed out.
PERF_PROCESS_GROUPS=()
declare -A PERF_PROCESS_PIDFILES=()
declare -A PERF_PROCESS_WATCHERS=()
declare -A PERF_PROCESS_WATCHFILES=()
perf_track_process_group() {
  PERF_PROCESS_GROUPS+=("$1")
  PERF_PROCESS_PIDFILES["$1"]="${2:-}"
}

perf_sandbox_off_default() {
  mkdir -p "$XDG_CONFIG_HOME/thegn"
  printf '\n[media]\nenabled = false\n' >>"$XDG_CONFIG_HOME/thegn/config.toml"
  if [ "${TG_PERF_SANDBOX:-}" != 1 ]; then
    printf '\n[sandbox]\nenabled = false\n' >>"$XDG_CONFIG_HOME/thegn/config.toml"
  fi
  printf '\n[sandbox.limits]\ncpu_total = "off"\nmemory_total = "off"\n' \
    >>"$XDG_CONFIG_HOME/thegn/config.toml"
}

perf_stop_process_group() {
  local pgid="$1" pidfile="${2:-}" pid="" tick ppid watcher tracked=0 target
  for target in "${PERF_PROCESS_GROUPS[@]}"; do
    if [ "$target" = "$pgid" ]; then
      tracked=1
      break
    fi
  done
  [ "$tracked" = 1 ] || return 0
  if [ -n "${PERF_PROCESS_WATCHERS[$pgid]:-}" ]; then
    watcher="${PERF_PROCESS_WATCHERS[$pgid]}"
    if [ "$(ps -o ppid= -p "$watcher" 2>/dev/null | tr -d ' ' || true)" = "$$" ]; then
      kill "$watcher" 2>/dev/null || true
      wait "$watcher" 2>/dev/null || true
    fi
    unset "PERF_PROCESS_WATCHERS[$pgid]"
  fi
  if [ -n "${PERF_PROCESS_WATCHFILES[$pgid]:-}" ]; then
    local watcher_sleep
    watcher_sleep="$(cat "${PERF_PROCESS_WATCHFILES[$pgid]}" 2>/dev/null || true)"
    [ -z "$watcher_sleep" ] || kill "$watcher_sleep" 2>/dev/null || true
    rm -f -- "${PERF_PROCESS_WATCHFILES[$pgid]}"
    unset "PERF_PROCESS_WATCHFILES[$pgid]"
  fi
  ppid="$(ps -o ppid= -p "$pgid" 2>/dev/null | tr -d ' ' || true)"
  if [ "$ppid" = "$$" ] && [ -n "$pidfile" ]; then
    pid="$(cat "$pidfile" 2>/dev/null || true)"
    tick=0
    while [ -z "$pid" ] && [ "$tick" -lt 10 ]; do
      sleep 0.1
      pid="$(cat "$pidfile" 2>/dev/null || true)"
      tick=$((tick + 1))
    done
    if [ -n "$pid" ]; then
      kill -TERM "$pid" 2>/dev/null || true
      for ((tick = 0; tick < 50; tick++)); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
      done
      if kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null || true
      fi
    fi
  fi
  if [ "$ppid" = "$$" ]; then
    kill -TERM -- "-$pgid" 2>/dev/null || true
    sleep 1
    kill -KILL -- "-$pgid" 2>/dev/null || true
    wait "$pgid" 2>/dev/null || true
  fi
  perf_untrack_process_group "$pgid"
}

perf_untrack_process_group() {
  local target="$1" i
  for i in "${!PERF_PROCESS_GROUPS[@]}"; do
    if [ "${PERF_PROCESS_GROUPS[$i]}" = "$target" ]; then
      unset 'PERF_PROCESS_GROUPS[i]'
    fi
  done
  unset "PERF_PROCESS_PIDFILES[$target]"
  unset "PERF_PROCESS_WATCHERS[$target]"
  unset "PERF_PROCESS_WATCHFILES[$target]"
  PERF_PROCESS_GROUPS=("${PERF_PROCESS_GROUPS[@]}")
}

perf_watch_process_group() {
  [ -z "${TIMEOUT_BIN:-}" ] || return 0
  local pgid="$1" deadline_s="$2" pidfile="${3:-}" watchfile="$PERF_TMP/perf-watch-$1.sleep"
  (
    sleep "$deadline_s" &
    local sleeper=$!
    trap 'kill "$sleeper" 2>/dev/null || true; wait "$sleeper" 2>/dev/null || true; exit 0' TERM
    printf '%s\n' "$sleeper" >"$watchfile"
    wait "$sleeper" 2>/dev/null || exit 0
    rm -f -- "$watchfile"
    if kill -0 "$pgid" 2>/dev/null; then
      local pid="" ppid tick
      ppid="$(ps -o ppid= -p "$pgid" 2>/dev/null | tr -d ' ' || true)"
      [ "$ppid" = "$$" ] || exit 0
      if [ -n "$pidfile" ]; then
        pid="$(cat "$pidfile" 2>/dev/null || true)"
        tick=0
        while [ -z "$pid" ] && [ "$tick" -lt 10 ]; do
          sleep 0.1
          pid="$(cat "$pidfile" 2>/dev/null || true)"
          tick=$((tick + 1))
        done
        if [ -n "$pid" ]; then
          kill -TERM "$pid" 2>/dev/null || true
          for ((tick = 0; tick < 50; tick++)); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
          done
          if kill -0 "$pid" 2>/dev/null; then
            kill -KILL "$pid" 2>/dev/null || true
          fi
        fi
      fi
      if [ "$ppid" = "$$" ]; then
        kill -TERM -- "-$pgid" 2>/dev/null || true
        sleep 1
        kill -KILL -- "-$pgid" 2>/dev/null || true
      fi
    fi
  ) &
  PERF_PROCESS_WATCHERS["$pgid"]=$!
  PERF_PROCESS_WATCHFILES["$pgid"]="$watchfile"
}

# Remove temporary files and any matching podman pause process owned by this
# fixture. Podman overlay layers may be owned by its user namespace, so remove
# them through `podman unshare` before trying ordinary rm.
perf_cleanup() {
  local d pgid ppid pidfile pid
  if [ "${#PERF_PROCESS_GROUPS[@]}" -gt 0 ]; then
    for pgid in "${PERF_PROCESS_GROUPS[@]}"; do
      ppid="$(ps -o ppid= -p "$pgid" 2>/dev/null | tr -d ' ' || true)"
      if [ "$ppid" = "$$" ]; then
        pidfile="${PERF_PROCESS_PIDFILES[$pgid]:-}"
        perf_stop_process_group "$pgid" "$pidfile"
      else
        perf_untrack_process_group "$pgid"
      fi
    done
  fi
  if [ -n "${PERF_SOCK_DIR:-}" ]; then
    rm -rf -- "$PERF_SOCK_DIR" || true
  fi
  if [ -n "${PERF_TMP:-}" ] && [ -e "$PERF_TMP" ]; then
    # Podman-owned overlay files may be inaccessible outside its user namespace.
    if [ -d "$PERF_TMP/home/.local/share/containers/storage" ] &&
      command -v podman >/dev/null 2>&1; then
      podman unshare rm -rf -- "$PERF_TMP" || true
    fi
    if [ -e "$PERF_TMP" ]; then
      rm -rf -- "$PERF_TMP" || true
    fi
  fi
  # `podman unshare` can start a new pause process with this fixture's runtime
  # environment, so scan after both deletion attempts. `/proc/PID/comm` holds
  # `podman pause`; cmdline contains only `podman`. Read races are silent.
  if [ -n "${PERF_TMP:-}" ] && [ -d "${PERF_PROC_ROOT:-/proc}" ]; then
    while IFS= read -r pid; do
      d="${PERF_PROC_ROOT:-/proc}/$pid"
      if tr '\0' '\n' 2>/dev/null <"$d/environ" | grep -F -- "$PERF_TMP/" >/dev/null; then
        kill "$pid" 2>/dev/null || true
      fi
    done < <(perf_pause_pids)
  fi
}

# Print pause-process PIDs using procfs comm (cmdline is only `podman`).
# Kept separate so the matcher can be exercised against an isolated proc tree.
perf_pause_pids() {
  local d comm proc_root="${PERF_PROC_ROOT:-/proc}"
  [ -d "$proc_root" ] || return 0
  for d in "$proc_root"/[0-9]*; do
    [ -d "$d" ] || continue
    if IFS= read -r comm 2>/dev/null <"$d/comm" && [ "$comm" = 'podman pause' ]; then
      printf '%s\n' "${d##*/}"
    fi
  done
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
