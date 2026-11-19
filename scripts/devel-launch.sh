#!/bin/sh
set -eu
# Dev panes find the dev `thegn` first on PATH (login shells that reset PATH still need $THEGN_BIN; `thegn doctor` names a mismatch).

checkout=$(cd "$(dirname "$0")/.." && pwd -P)
home=$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6 || true)
[ -n "$home" ] || home="$HOME"

root="$home/.thegn-devel"
state="$root/state"
run="$root/run"
cfg="$home/.config/thegn-dev"
bin="${THEGN_DEVEL_BIN:-$root/bin/thegn}"
bindir=$(dirname "$bin")
pane_vars="THEGN_SESSION_ID THEGN_CONTROL_SOCKET THEGN_WORKTREE THEGN_BRANCH THEGN_WORKSPACE THEGN_REPO_ROOT THEGN_SANDBOX THEGN_SANDBOX_NETWORK THEGN_SANDBOX_ENABLED THEGN_FORKED_FROM THEGN_FORK_SCROLLBACK THEGN_ROUTE_TOKEN THEGN_ROUTE_ORIGIN THEGN_PROMPT THEGN_EVENT THEGN_BIN"

dev_path() { # the dev binary's dir first on PATH, once
  case ":${PATH:-}:" in
  ":$bindir:"*) printf '%s' "${PATH:-}" ;;
  *) printf '%s' "$bindir${PATH:+:$PATH}" ;;
  esac
}

host_session_env() {
  real_run=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
  [ "$real_run" = "$run" ] && real_run="/run/user/$(id -u)"

  _devel_wayland_rewritten=
  _devel_dbus_rewritten=
  if [ -n "${WAYLAND_DISPLAY:-}" ] && [ "${WAYLAND_DISPLAY#/}" = "$WAYLAND_DISPLAY" ]; then
    WAYLAND_DISPLAY="$real_run/$WAYLAND_DISPLAY"
    export WAYLAND_DISPLAY
    _devel_wayland_rewritten=1
  fi
  if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] && [ -S "$real_run/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS="unix:path=$real_run/bus"
    export DBUS_SESSION_BUS_ADDRESS
    _devel_dbus_rewritten=1
  fi
}

print_env() {
  host_session_env
  printf 'unset %s\n' "$pane_vars"
  printf "export %s='%s'\n" XDG_STATE_HOME "$state"
  printf "export %s='%s'\n" XDG_RUNTIME_DIR "$real_run"
  printf "export %s='%s'\n" THEGN_DIR "$root/home"
  printf "export %s='%s'\n" THEGN_CONFIG_DIR "$cfg"
  printf "export %s='%s'\n" THEGN_NO_MIGRATE 1
  printf "export %s='%s'\n" THEGN_CHANNEL dev
  printf "export %s='%s'\n" PATH "$(dev_path)"
  # THEGN_BIN is for scripts (the dev binary's absolute path); thegn itself does not read it.
  printf "export %s='%s'\n" THEGN_BIN "$bin"
  if [ -n "$_devel_wayland_rewritten" ]; then
    printf "export %s='%s'\n" WAYLAND_DISPLAY "$WAYLAND_DISPLAY"
  fi
  if [ -n "$_devel_dbus_rewritten" ]; then
    printf "export %s='%s'\n" DBUS_SESSION_BUS_ADDRESS "$DBUS_SESSION_BUS_ADDRESS"
  fi
}

desktop_entry() {
  case "$checkout" in
  *[!A-Za-z0-9/._-]*)
    echo "devel-launch: checkout path contains characters that cannot be represented in a desktop Exec line: $checkout" >&2
    exit 2
    ;;
  esac
  printf '%s\n' \
    '[Desktop Entry]' \
    'Type=Application' \
    'Version=1.0' \
    'Name=thegn (dev)' \
    'GenericName=Git Worktree IDE (dev build)' \
    "Comment=thegn dev instance built from $checkout" \
    "Exec=$checkout/scripts/devel-launch.sh --standalone" \
    "TryExec=$checkout/scripts/devel-launch.sh" \
    'Terminal=false' \
    'Icon=thegn' \
    'Categories=Development;' \
    'StartupWMClass=thegn-dev' \
    'X-Thegn-Dev=true'
  echo '# save with: just devel-desktop > ~/.local/share/applications/thegn-dev.desktop' >&2
}

standalone() {
  if ! command -v alacritty >/dev/null 2>&1; then
    echo "devel-launch: alacritty not found; run 'just devel' in a terminal instead" >&2
    exit 127
  fi
  exec alacritty \
    --class thegn-dev \
    --title "thegn (dev)" \
    --config-file "$checkout/config/alacritty.toml" \
    -e "$checkout/scripts/devel-launch.sh"
}

check_launch() {
  if [ ! -x "$bin" ]; then
    echo "devel-launch: $bin is missing; build it with 'just live' (release) or 'just devel' (debug)" >&2
    exit 1
  fi
  seed_config
}

dev_ports() {
  awk '
  function close_table() {
    if (table == "serve" && !table_done) print "bind = \"127.0.0.1:5381\""
    if (table == "model_proxy" && !table_done) print "listen = \"127.0.0.1:8384\""
  }
  function shifted_port(line,    port) {
    if (match(line, /:[0-9]+"/)) {
      port = substr(line, RSTART + 1, RLENGTH - 2)
      return substr(line, 1, RSTART) (port + 1) "\"" substr(line, RSTART + RLENGTH)
    }
    return line
  }
  function warn(message) {
    print message | "cat >&2"
    close("cat >&2")
  }
  {
    if ($0 ~ /^[ \t]*model_proxy[ \t]*[.=]/) model_proxy_key = 1
    if ($0 ~ /^[ \t]*serve[ \t]*[.=]/) serve_key = 1

    if ($0 ~ /^[ \t]*\[\[/) {
      close_table()
      table = ""
      table_done = 0
      print
      next
    }
    if ($0 ~ /^[ \t]*\[[^]]*\][ \t]*(#.*)?$/) {
      close_table()
      table = $0
      sub(/^[ \t]*\[/, "", table)
      sub(/\][ \t]*(#.*)?$/, "", table)
      gsub(/[ \t]/, "", table)
      table_done = 0
      if (table == "serve") serve_seen = 1
      if (table == "model_proxy") model_proxy_seen = 1
      print
      next
    }
    if (table == "serve" && !table_done && $0 ~ /^[ \t]*bind[ \t]*=/) {
      table_done = 1
      print shifted_port($0)
      next
    }
    if (table == "model_proxy" && !table_done && $0 ~ /^[ \t]*listen[ \t]*=/) {
      table_done = 1
      print shifted_port($0)
      next
    }
    print
  }
  END {
    close_table()
    if (!serve_seen) {
      if (serve_key) warn("devel-launch: set the dev [serve] bind port by hand")
      else {
        print ""
        print "[serve]"
        print "bind = \"127.0.0.1:5381\""
      }
    }
    if (!model_proxy_seen) {
      if (model_proxy_key) warn("devel-launch: set the dev [model_proxy] listen port by hand")
      else {
        print ""
        print "[model_proxy]"
        print "listen = \"127.0.0.1:8384\""
      }
    }
  }'
}

seed_config() {
  [ -d "$cfg" ] && return 0
  src="$home/.config/thegn/config.toml"
  if ! mkdir -m 700 "$cfg" 2>/dev/null; then
    [ -d "$cfg" ] && return 0
    echo "devel-launch: cannot create $cfg" >&2
    exit 1
  fi
  tmp="$cfg/.config.toml.seed"
  {
    printf '# DEV instance config, seeded %s by scripts/devel-launch.sh from %s.\n' "$(date -u +%Y-%m-%d)" "$src"
    printf '# Only the dev instance reads it (THEGN_CONFIG_DIR). Ports are shifted by one; secrets/ and nix-signing/ were not copied.\n'
    if [ -r "$src" ]; then dev_ports <"$src"; else dev_ports </dev/null; fi
  } >"$tmp" || {
    rm -f "$tmp"
    exit 1
  }
  chmod 600 "$tmp" && mv -f "$tmp" "$cfg/config.toml"
  echo "devel-launch: seeded $cfg/config.toml from $src (review it; the keyring service is shared)" >&2
}

launch() {
  check_launch
  host_session_env
  pidfile="$run/thegn.pid"
  mkdir -p "$state" "$run" "$root/home"
  # shellcheck source=test/lib/instance.sh disable=SC1091 # resolved at run time from the checkout
  . "$checkout/test/lib/instance.sh"
  INST_STATE="$state" inst_restart "$pidfile" $$ || exit 1
  set --
  for pane_var in $pane_vars; do
    set -- "$@" -u "$pane_var"
  done
  set -- "$@" \
    "THEGN_ALACRITTY_CONFIG=$checkout/config/alacritty.toml" \
    "XDG_STATE_HOME=$state" \
    "XDG_RUNTIME_DIR=$real_run" \
    "THEGN_DIR=$root/home" \
    "THEGN_CONFIG_DIR=$cfg" \
    "THEGN_NO_MIGRATE=1" \
    "THEGN_CHANNEL=dev" \
    "PATH=$(dev_path)"
  if [ "${WAYLAND_DISPLAY+x}" = x ]; then
    set -- "$@" "WAYLAND_DISPLAY=$WAYLAND_DISPLAY"
  fi
  if [ "${DBUS_SESSION_BUS_ADDRESS+x}" = x ]; then
    set -- "$@" "DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"
  fi
  exec env "$@" "$bin"
}

daemon() {
  check_launch
  host_session_env
  mkdir -p "$state" "$run" "$root/home"
  set --
  for pane_var in $pane_vars; do
    set -- "$@" -u "$pane_var"
  done
  set -- "$@" \
    "THEGN_ALACRITTY_CONFIG=$checkout/config/alacritty.toml" \
    "XDG_STATE_HOME=$state" \
    "XDG_RUNTIME_DIR=$real_run" \
    "THEGN_DIR=$root/home" \
    "THEGN_CONFIG_DIR=$cfg" \
    "THEGN_NO_MIGRATE=1" \
    "THEGN_CHANNEL=dev" \
    "PATH=$(dev_path)"
  if [ "${WAYLAND_DISPLAY+x}" = x ]; then
    set -- "$@" "WAYLAND_DISPLAY=$WAYLAND_DISPLAY"
  fi
  if [ "${DBUS_SESSION_BUS_ADDRESS+x}" = x ]; then
    set -- "$@" "DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"
  fi
  if command -v setsid >/dev/null 2>&1; then
    setsid -f env "$@" "$bin" daemon >"$root/daemon.out" 2>&1 </dev/null
  else
    nohup env "$@" "$bin" daemon >"$root/daemon.out" 2>&1 </dev/null &
  fi
  echo "devel-launch: dev daemon started; log $root/daemon.out" >&2
}

case "${1-}" in
--print-env)
  [ "$#" -eq 1 ] || {
    echo "devel-launch: --print-env takes no arguments" >&2
    exit 2
  }
  print_env
  ;;
--desktop-entry)
  [ "$#" -eq 1 ] || {
    echo "devel-launch: --desktop-entry takes no arguments" >&2
    exit 2
  }
  desktop_entry
  ;;
--standalone)
  [ "$#" -eq 1 ] || {
    echo "devel-launch: --standalone takes no arguments" >&2
    exit 2
  }
  standalone
  ;;
--daemon)
  [ "$#" -eq 1 ] || {
    echo "devel-launch: --daemon takes no arguments" >&2
    exit 2
  }
  daemon
  ;;
'')
  launch
  ;;
*)
  echo "devel-launch: unknown argument: $1" >&2
  exit 2
  ;;
esac
