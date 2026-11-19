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
contains "$env_output" "export THEGN_BIN='$root/bin/thegn'"

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

mkdir -p "$tmp/home/.config/thegn-dev" "$tmp/bin" "$tmp/stub"
cat >"$tmp/stub/thegn" <<'EOF'
#!/bin/sh
env >"$STUB_OUT"
EOF
chmod +x "$tmp/stub/thegn"
cat >"$tmp/bin/getent" <<'EOF'
#!/bin/sh
exit 1
EOF
chmod +x "$tmp/bin/getent"

dev_print_env="$(STUB_OUT="$tmp/print-env.stub.env" HOME="$tmp/home" PATH="$tmp/bin:$PATH" \
  THEGN_DEVEL_BIN="$tmp/stub/thegn" "$launcher" --print-env)"
contains "$dev_print_env" "export PATH='$tmp/stub:$tmp/bin:"

dedup_print_env="$(STUB_OUT="$tmp/dedup.stub.env" HOME="$tmp/home" PATH="$tmp/stub:$tmp/bin:$PATH" \
  THEGN_DEVEL_BIN="$tmp/stub/thegn" "$launcher" --print-env)"
contains "$dedup_print_env" "export PATH='$tmp/stub:$tmp/bin:"
[[ $dedup_print_env != *"export PATH='$tmp/stub:$tmp/stub:"* ]] || fail 'dev binary directory was duplicated on PATH'

STUB_OUT="$tmp/stub.env" HOME="$tmp/home" PATH="$tmp/bin:$PATH" \
  THEGN_DEVEL_BIN="$tmp/stub/thegn" THEGN_SESSION_ID=x XDG_RUNTIME_DIR="$tmp/run0" \
  "$launcher"
if grep -q '^THEGN_SESSION_ID=' "$tmp/stub.env"; then
  fail 'launch leaked THEGN_SESSION_ID'
fi
stub_env="$(<"$tmp/stub.env")"
contains "$stub_env" "XDG_RUNTIME_DIR=$tmp/run0"
contains "$stub_env" 'THEGN_CHANNEL=dev'
contains "$stub_env" "PATH=$tmp/stub:"

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
if HOME="$tmp/home" PATH="$tmp/bin:$PATH" THEGN_DEVEL_BIN="$tmp/missing" \
  "$tmp/co/scripts/devel-launch.sh" >"$tmp/missing.out" 2>"$tmp/missing.err"; then
  fail 'launch without a binary succeeded'
else
  missing_rc=$?
  [[ $missing_rc -eq 1 ]] || fail "missing binary returned $missing_rc, want 1"
fi
contains "$(<"$tmp/missing.err")" "just devel"
contains "$(<"$tmp/missing.err")" "just live"

if HOME="$tmp/home" PATH="$tmp/bin:$PATH" THEGN_DEVEL_BIN="$tmp/missing" \
  "$launcher" --daemon >"$tmp/daemon.out" 2>"$tmp/daemon.err"; then
  fail 'daemon launch without a binary succeeded'
else
  daemon_rc=$?
  [[ $daemon_rc -eq 1 ]] || fail "missing daemon binary returned $daemon_rc, want 1"
fi
contains "$(<"$tmp/daemon.err")" 'is missing'

mkdir -p "$tmp/h2/.config/thegn"
printf '%s\n' '[serve]' 'bind = "127.0.0.1:5380"' '[ui]' 'x = 1' >"$tmp/h2/.config/thegn/config.toml"
cp "$tmp/h2/.config/thegn/config.toml" "$tmp/h2-stable.copy"
STUB_OUT="$tmp/h2.stub.env" HOME="$tmp/h2" PATH="$tmp/bin:$PATH" \
  THEGN_DEVEL_BIN="$tmp/stub/thegn" XDG_RUNTIME_DIR="$tmp/h2-run" "$launcher"
h2_cfg="$tmp/h2/.config/thegn-dev/config.toml"
[[ -f $h2_cfg ]] || fail 'stable config was not seeded'
[[ $(stat -c %a "$h2_cfg") == 600 ]] || fail 'seeded config is not mode 600'
contains "$(<"$h2_cfg")" 'bind = "127.0.0.1:5381"'
contains "$(<"$h2_cfg")" '[model_proxy]'
contains "$(<"$h2_cfg")" 'listen = "127.0.0.1:8384"'
contains "$(<"$h2_cfg")" 'x = 1'
cmp "$tmp/h2/.config/thegn/config.toml" "$tmp/h2-stable.copy"

printf '%s\n' '# mine' >>"$h2_cfg"
STUB_OUT="$tmp/h2-second.stub.env" HOME="$tmp/h2" PATH="$tmp/bin:$PATH" \
  THEGN_DEVEL_BIN="$tmp/stub/thegn" XDG_RUNTIME_DIR="$tmp/h2-run" "$launcher"
contains "$(<"$h2_cfg")" '# mine'

mkdir -p "$tmp/h3/.config/thegn"
printf '%s\n' '[serve]' 'bind = "127.0.0.1:5380"' >"$tmp/h3/stable.toml"
chmod 0444 "$tmp/h3/stable.toml"
ln -s "$tmp/h3/stable.toml" "$tmp/h3/.config/thegn/config.toml"
STUB_OUT="$tmp/h3.stub.env" HOME="$tmp/h3" PATH="$tmp/bin:$PATH" \
  THEGN_DEVEL_BIN="$tmp/stub/thegn" XDG_RUNTIME_DIR="$tmp/h3-run" "$launcher"
h3_cfg="$tmp/h3/.config/thegn-dev/config.toml"
[[ -f $h3_cfg && ! -L $h3_cfg ]] || fail 'symlink source did not produce a regular seed'
[[ $(stat -c %a "$h3_cfg") == 600 ]] || fail 'symlink seed is not mode 600'

mkdir -p "$tmp/h4/.config"
STUB_OUT="$tmp/h4.stub.env" HOME="$tmp/h4" PATH="$tmp/bin:$PATH" \
  THEGN_DEVEL_BIN="$tmp/stub/thegn" XDG_RUNTIME_DIR="$tmp/h4-run" "$launcher"
h4_cfg="$tmp/h4/.config/thegn-dev/config.toml"
contains "$(<"$h4_cfg")" '[serve]'
contains "$(<"$h4_cfg")" 'bind = "127.0.0.1:5381"'
contains "$(<"$h4_cfg")" '[model_proxy]'
contains "$(<"$h4_cfg")" 'listen = "127.0.0.1:8384"'

mkdir -p "$tmp/h5/.config/thegn"
printf '%s\n' '[serve]' 'topology = "direct"' >"$tmp/h5/.config/thegn/config.toml"
STUB_OUT="$tmp/h5.stub.env" HOME="$tmp/h5" PATH="$tmp/bin:$PATH" \
  THEGN_DEVEL_BIN="$tmp/stub/thegn" XDG_RUNTIME_DIR="$tmp/h5-run" "$launcher"
h5_cfg="$tmp/h5/.config/thegn-dev/config.toml"
[[ $(grep -c '^\[serve\]$' "$h5_cfg") -eq 1 ]] || fail 'seed duplicated the serve table'
[[ $(grep -c '^bind = "127.0.0.1:5381"$' "$h5_cfg") -eq 1 ]] || fail 'seed did not add one serve bind'

readonly_home="$tmp/readonly-home"
mkdir -p "$readonly_home"
PATH="$tmp/bin:$PATH" HOME="$readonly_home" THEGN_DEVEL_BIN="$tmp/stub/thegn" \
  "$launcher" --print-env >/dev/null
PATH="$tmp/bin:$PATH" HOME="$readonly_home" THEGN_DEVEL_BIN="$tmp/stub/thegn" \
  "$launcher" --desktop-entry >/dev/null
[[ ! -e $readonly_home/.config/thegn-dev ]] || fail 'read-only mode created dev config'

echo 'devel launch checks passed'
