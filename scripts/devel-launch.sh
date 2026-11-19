#!/bin/sh
set -eu

checkout=$(cd "$(dirname "$0")/.." && pwd -P)
home=$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6 || true)
[ -n "$home" ] || home="$HOME"

root="$home/.thegn-devel"
state="$root/state"
run="$root/run"
cfg="$home/.config/thegn-dev"
bin="$checkout/target/debug/thegn"

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
  printf "export %s='%s'\n" XDG_STATE_HOME "$state"
  printf "export %s='%s'\n" XDG_RUNTIME_DIR "$run"
  printf "export %s='%s'\n" THEGN_DIR "$root/home"
  printf "export %s='%s'\n" THEGN_CONFIG_DIR "$cfg"
  printf "export %s='%s'\n" THEGN_NO_MIGRATE 1
  printf "export %s='%s'\n" THEGN_CHANNEL dev
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

launch() {
  if [ ! -x "$bin" ]; then
    echo "devel-launch: $bin is missing; build it first with 'just devel' (builds and starts) or 'cargo build'" >&2
    exit 1
  fi
  if [ ! -d "$cfg" ]; then
    echo "just devel: $cfg does not exist; seed it first: cp -r '$home/.config/thegn' '$cfg' (review it for secrets)" >&2
    exit 1
  fi
  host_session_env
  pidfile="$run/thegn.pid"
  mkdir -p "$state" "$run" "$root/home"
  # shellcheck source=test/lib/instance.sh disable=SC1091 # resolved at run time from the checkout
  . "$checkout/test/lib/instance.sh"
  INST_STATE="$state" inst_restart "$pidfile" $$ || exit 1
  set -- \
    "THEGN_ALACRITTY_CONFIG=$checkout/config/alacritty.toml" \
    "XDG_STATE_HOME=$state" \
    "XDG_RUNTIME_DIR=$run" \
    "THEGN_DIR=$root/home" \
    "THEGN_CONFIG_DIR=$cfg" \
    "THEGN_NO_MIGRATE=1" \
    "THEGN_CHANNEL=dev"
  if [ "${WAYLAND_DISPLAY+x}" = x ]; then
    set -- "$@" "WAYLAND_DISPLAY=$WAYLAND_DISPLAY"
  fi
  if [ "${DBUS_SESSION_BUS_ADDRESS+x}" = x ]; then
    set -- "$@" "DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"
  fi
  exec env "$@" "$bin"
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
'')
  launch
  ;;
*)
  echo "devel-launch: unknown argument: $1" >&2
  exit 2
  ;;
esac
