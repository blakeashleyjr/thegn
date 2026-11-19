#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
launcher="$repo/scripts/devel-launch.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

contains() {
  local haystack=$1 needle=$2
  [[ $haystack == *"$needle"* ]] || fail "expected '$needle'"
}

home="$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6 || true)"
[[ -n $home ]] || home=$HOME
root="$home/.thegn-devel"

root_before=0
[[ -e $root ]] && root_before=1
before_entries=''
if [[ $root_before -eq 1 ]]; then
  before_entries="$(find "$root" -mindepth 1 -maxdepth 1 -printf '%P\n' | sort)"
fi

env_output="$(XDG_RUNTIME_DIR="$tmp/run0" WAYLAND_DISPLAY=wayland-9 DBUS_SESSION_BUS_ADDRESS='' \
  "$launcher" --print-env)"
contains "$env_output" "export WAYLAND_DISPLAY='$tmp/run0/wayland-9'"
contains "$env_output" "export XDG_RUNTIME_DIR='$tmp/run0'"

scrub_output="$(THEGN_SESSION_ID=x THEGN_ROUTE_TOKEN=y "$launcher" --print-env)"
scrub_first_line="${scrub_output%%$'\n'*}"
contains "$scrub_first_line" 'unset THEGN_SESSION_ID'
contains "$scrub_first_line" 'THEGN_ROUTE_TOKEN'
[[ $(grep -c '^unset ' <<<"$scrub_output") -eq 1 ]] || fail 'expected one unset line'
[[ $scrub_output != *'THEGN_SESSION_ID=x'* ]] || fail 'session id leaked from --print-env'
[[ $scrub_output != *'THEGN_ROUTE_TOKEN=y'* ]] || fail 'route token leaked from --print-env'

if [[ $root_before -eq 1 ]]; then
  after_entries="$(find "$root" -mindepth 1 -maxdepth 1 -printf '%P\n' | sort)"
  [[ $before_entries == "$after_entries" ]] || fail "--print-env changed $root"
else
  [[ ! -e $root ]] || fail "--print-env created $root"
fi

absolute_output="$(XDG_RUNTIME_DIR="$tmp/run0" WAYLAND_DISPLAY=/x/w DBUS_SESSION_BUS_ADDRESS='' \
  "$launcher" --print-env)"
[[ $absolute_output != *"$tmp/run0/x/w"* ]] || fail "absolute WAYLAND_DISPLAY was rewritten"
[[ $absolute_output != *"export WAYLAND_DISPLAY="* ]] || fail "absolute WAYLAND_DISPLAY was unexpectedly emitted"

dbus_output="$(XDG_RUNTIME_DIR="$tmp/run0" WAYLAND_DISPLAY='' DBUS_SESSION_BUS_ADDRESS=unix:path=/already-set \
  "$launcher" --print-env)"
[[ $dbus_output != *"export DBUS_SESSION_BUS_ADDRESS="* ]] || fail "existing DBUS address was emitted"

desktop_output="$("$launcher" --desktop-entry 2>"$tmp/desktop.err")"
contains "$desktop_output" "Exec=$repo/scripts/devel-launch.sh --standalone"
contains "$desktop_output" 'Terminal=false'
contains "$desktop_output" 'StartupWMClass=thegn-dev'
[[ $desktop_output != *'~'* && $desktop_output != *'$'* ]] || fail 'desktop entry contains shell shorthand'
contains "$(<"$tmp/desktop.err")" '# save with: just devel-desktop > ~/.local/share/applications/thegn-dev.desktop'

mkdir -p "$tmp/home/.config/thegn-dev" "$tmp/bin"
cat >"$tmp/stub" <<'EOF'
#!/bin/sh
env >"$STUB_OUT"
EOF
chmod +x "$tmp/stub"
cat >"$tmp/bin/getent" <<'EOF'
#!/bin/sh
exit 1
EOF
chmod +x "$tmp/bin/getent"

STUB_OUT="$tmp/stub.env" HOME="$tmp/home" PATH="$tmp/bin:$PATH" \
  THEGN_DEVEL_BIN="$tmp/stub" THEGN_SESSION_ID=x XDG_RUNTIME_DIR="$tmp/run0" \
  "$launcher"
if grep -q '^THEGN_SESSION_ID=' "$tmp/stub.env"; then
  fail 'launch leaked THEGN_SESSION_ID'
fi
stub_env="$(<"$tmp/stub.env")"
contains "$stub_env" "XDG_RUNTIME_DIR=$tmp/run0"
contains "$stub_env" 'THEGN_CHANNEL=dev'

mkdir -p "$tmp/a b/scripts"
cp "$launcher" "$tmp/a b/scripts/devel-launch.sh"
chmod +x "$tmp/a b/scripts/devel-launch.sh"
if "$tmp/a b/scripts/devel-launch.sh" --desktop-entry >"$tmp/spaced.out" 2>"$tmp/spaced.err"; then
  fail 'desktop entry accepted a checkout path containing a space'
else
  spaced_rc=$?
  [[ $spaced_rc -eq 2 ]] || fail "spaced checkout returned $spaced_rc, want 2"
fi
[[ -s $tmp/spaced.err ]] || fail 'spaced checkout did not explain the refusal'

mkdir -p "$tmp/co/scripts"
cp "$launcher" "$tmp/co/scripts/devel-launch.sh"
chmod +x "$tmp/co/scripts/devel-launch.sh"
if "$tmp/co/scripts/devel-launch.sh" >"$tmp/missing.out" 2>"$tmp/missing.err"; then
  fail 'launch without a binary succeeded'
else
  missing_rc=$?
  [[ $missing_rc -eq 1 ]] || fail "missing binary returned $missing_rc, want 1"
fi
contains "$(<"$tmp/missing.err")" "just devel"

if THEGN_DEVEL_BIN="$tmp/missing" "$launcher" --daemon >"$tmp/daemon.out" 2>"$tmp/daemon.err"; then
  fail 'daemon launch without a binary succeeded'
else
  daemon_rc=$?
  [[ $daemon_rc -eq 1 ]] || fail "missing daemon binary returned $daemon_rc, want 1"
fi
contains "$(<"$tmp/daemon.err")" 'is missing'

echo 'devel launch checks passed'
