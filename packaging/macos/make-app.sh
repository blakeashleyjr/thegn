#!/usr/bin/env bash
# make-app.sh — generate a macOS `thegn.app` launcher bundle.
#
# thegn is a TUI, so the bundle is a launcher: double-clicking it (Spotlight,
# Raycast, Alfred, the Dock, /Applications) opens a terminal emulator running
# `thegn`. It is the Darwin counterpart to the `.desktop` entry `install.sh`
# writes on Linux/BSD — same job, different registry.
#
# The bundle is generated LOCALLY (by install.sh, `just macos-app`, or by hand),
# never downloaded. That matters: a locally created bundle carries no
# `com.apple.quarantine` xattr, so Gatekeeper does not block it and no code
# signing / notarization is involved. Shipping a prebuilt .app would need both.
#
#   ./packaging/macos/make-app.sh --bin ~/.local/bin/thegn
#   ./packaging/macos/make-app.sh --bin "$(command -v thegn)" --dest /Applications
#
# Re-running replaces an installer-owned bundle after building and validating a
# complete replacement beside it.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat >&2 <<'EOF'
usage: make-app.sh [options]

  --bin PATH                thegn binary to launch (default: `thegn` on PATH)
  --dest DIR                where to write the bundle (default: ~/Applications)
  --name NAME               bundle/app name (default: thegn)
  --icon PATH               .icns icon (default: packaging/macos/thegn.icns)
  --terminal PATH           preferred terminal binary; the launcher probes for
                            one at runtime if unset or missing
  --alacritty-config PATH   bundled alacritty profile to use when the chosen
                            terminal is alacritty
  --env KEY=VALUE           export KEY in the launched session (repeatable).
                            e.g. --env THEGN_LOG=info --env THEGN_PERF=1 for a
                            watchable build, or --env THEGN_CHANNEL=dev
  --dry-run                 print the plan without writing anything
EOF
}

bin=""
dest="$HOME/Applications"
name="thegn"
icon="$here/thegn.icns"
terminal=""
alacritty_config=""
envs=()
dry_run=0

while (($#)); do
  case "$1" in
  --bin)
    bin="${2-}"
    shift
    ;;
  --dest)
    dest="${2-}"
    shift
    ;;
  --name)
    name="${2-}"
    shift
    ;;
  --icon)
    icon="${2-}"
    shift
    ;;
  --terminal)
    terminal="${2-}"
    shift
    ;;
  --alacritty-config)
    alacritty_config="${2-}"
    shift
    ;;
  --env)
    if [[ ${2-} != *=* ]]; then
      echo "make-app.sh: --env wants KEY=VALUE, got: ${2-}" >&2
      exit 2
    fi
    envs+=("$2")
    shift
    ;;
  --dry-run) dry_run=1 ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "make-app.sh: unknown argument: $1" >&2
    usage
    exit 2
    ;;
  esac
  shift
done

# These values are embedded in bundle filenames, plist XML, and generated shell.
# The restricted grammars make those interpolations literal and prevent a key
# from introducing shell syntax into the generated `export KEY=...` command.
if [[ ! $name =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ || $name == . || $name == .. ]]; then
  echo "make-app.sh: --name must be 1-64 ASCII letters, digits, dots, underscores, or hyphens and start with a letter or digit" >&2
  exit 2
fi
for kv in ${envs+"${envs[@]}"}; do
  env_key="${kv%%=*}"
  if [[ ! $env_key =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
    echo "make-app.sh: invalid --env key: $env_key" >&2
    exit 2
  fi
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "make-app.sh: macOS only (on Linux/BSD install.sh writes a .desktop entry)" >&2
  exit 1
fi

if [[ -z $bin ]]; then
  bin="$(command -v thegn || true)"
fi
if [[ -z $bin ]]; then
  echo "make-app.sh: no thegn binary given and none on PATH — pass --bin PATH" >&2
  exit 1
fi
# Absolute + resolved: the launcher runs with launchd's PATH and no cwd of ours.
# A `~/.nix-profile` or `~/.local/bin` path is kept as-is (it survives upgrades);
# only a relative path is made absolute.
[[ $bin == /* ]] || bin="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"
if [[ ! -x $bin ]]; then
  echo "make-app.sh: not executable: $bin" >&2
  exit 1
fi

app="$dest/$name.app"
version_output="$("$bin" --version 2>/dev/null)" || {
  echo "make-app.sh: could not read version from $bin" >&2
  exit 1
}
if [[ ! $version_output =~ ^thegn\ ([0-9]+(\.[0-9]+){0,2}(-[0-9A-Za-z.]+)?)$ ]]; then
  echo "make-app.sh: unsupported version output from $bin" >&2
  exit 1
fi
version="${BASH_REMATCH[1]}"
# CFBundleVersion must be numeric-dotted; a pre-release suffix (0.1.0-alpha.2)
# is only legal in CFBundleShortVersionString.
short_version="$version"
build_version="${version%%-*}"

if ((dry_run)); then
  echo "dry-run: no files will be changed"
  echo "$app"
  echo "  Contents/MacOS/$name        -> launcher (opens a terminal running $bin)"
  echo "  Contents/Resources/env      <- launcher environment data"
  echo "  Contents/Resources/launch.sh -> static environment loader"
  echo "  Contents/Resources/$name.icns <- $icon"
  echo "  Contents/Resources/$name.command -> Terminal.app fallback runner"
  echo "  Contents/Info.plist         -> CFBundleShortVersionString $short_version"
  exit 0
fi

if [[ -L $dest ]]; then
  echo "make-app.sh: destination is a symlink: $dest" >&2
  exit 1
fi
if [[ -e $dest && ! -d $dest ]]; then
  echo "make-app.sh: destination is not a directory: $dest" >&2
  exit 1
fi
mkdir -p "$dest"
dest="$(cd -P -- "$dest" && pwd -P)"
app="$dest/$name.app"

if [[ -L $app ]]; then
  echo "make-app.sh: app bundle is a symlink: $app" >&2
  exit 1
fi
if [[ -e $app && ! -d $app ]]; then
  echo "make-app.sh: app bundle is not a directory: $app" >&2
  exit 1
fi
if [[ -e $app ]]; then
  marker="$app/Contents/Resources/.thegn-make-app"
  legacy_common="$app/Contents/Resources/common.sh"
  legacy_plist="$app/Contents/Info.plist"
  if [[ ! -f $marker || -L $marker ]]; then
    if [[ ! -f $legacy_common || -L $legacy_common || ! -f $legacy_plist || -L $legacy_plist ]] ||
      ! grep -Fq "<string>com.github.blakeashleyjr.$name</string>" "$legacy_plist"; then
      echo "make-app.sh: existing $app was not generated by make-app.sh; move it aside first" >&2
      exit 1
    fi
  fi
fi

staging=""
old=""
cleanup() {
  if [[ -n $staging && -e $staging ]]; then
    rm -rf -- "$staging"
  fi
  if [[ -n $old && -d $old && ! -e $old/app ]]; then
    rmdir "$old" 2>/dev/null || true
  elif [[ -n $old && -e $old/app ]]; then
    echo "make-app.sh: previous bundle remains at $old/app after rollback failure" >&2
  fi
}
trap cleanup EXIT

staging="$(mktemp -d "$dest/.$name.app.staging.XXXXXXXX")"
mkdir -p "$staging/Contents/MacOS" "$staging/Contents/Resources"

login_shell="${SHELL:-/bin/zsh}"

q() { printf '%q' "$1"; }
bin_q="$(q "$bin")"

# ── Contents/Resources/common.sh — shared by both entry points ────────────────
# Paths are baked into common.sh; environment values remain data in Resources/env.
{
  cat <<EOF
#!/bin/bash
# Generated by packaging/macos/make-app.sh — do not edit; re-run the generator.
THEGN_BIN=$bin_q
THEGN_LOGIN_SHELL=$(q "$login_shell")
THEGN_ALACRITTY_CFG=$(q "$alacritty_config")
APP_NAME=$(q "$name")
EOF
  cat <<'EOF'

THEGN_RES="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# A GUI launch inherits launchd's PATH, which contains none of the usual install
# prefixes (nix profiles, ~/.local/bin, Homebrew). Everything here is resolved by
# absolute path instead.
THEGN_DIRS=(
  "$(dirname "$THEGN_BIN")"
  "$HOME/.nix-profile/bin"
  "/etc/profiles/per-user/$USER/bin"
  "/run/current-system/sw/bin"
  "$HOME/.local/bin"
  "/opt/homebrew/bin"
  "/usr/local/bin"
  "$HOME/.cargo/bin"
)

# GUI launches have nowhere to print to, so a failure has to be shown, not
# logged — otherwise the icon bounces once and nothing happens.
alert() {
  /usr/bin/osascript -e "display alert \"$APP_NAME\" message \"$1\" as critical" >/dev/null 2>&1 || true
  echo "$APP_NAME: $1" >&2
}

# Echo a usable thegn binary. The baked path wins; the search covers the case
# where the install moved (a re-installed nix profile, a rebuilt ~/.local/bin).
resolve_bin() {
  if [ -x "$THEGN_BIN" ]; then
    echo "$THEGN_BIN"
    return 0
  fi
  for d in "${THEGN_DIRS[@]}"; do
    if [ -x "$d/thegn" ]; then
      echo "$d/thegn"
      return 0
    fi
  done
  return 1
}

# Export every KEY=VALUE record of the NUL-separated env file as data, without executing generated source.
load_env() {
  local kv key
  [ -f "$THEGN_RES/env" ] && [ ! -L "$THEGN_RES/env" ] || { alert "launcher environment file missing: $THEGN_RES/env"; return 1; }
  while IFS= read -r -d '' kv; do
    key="${kv%%=*}"
    [[ $key =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || { alert "invalid launcher environment entry in $THEGN_RES/env"; return 1; }
    export "$key=${kv#*=}"
  done <"$THEGN_RES/env"
  if [[ -n $kv ]]; then
    alert "unterminated launcher environment entry in $THEGN_RES/env"
    return 1
  fi
}

# The command line a terminal is asked to run: a LOGIN shell, so thegn sees the
# same PATH as an interactive terminal. The login profile runs first; launch.sh
# then exports the baked env data, so those values retain precedence.
#
# $2 is the resolved terminal (a path, or empty for the Terminal.app fallback).
# THEGN_ALACRITTY_CONFIG is exported ONLY when Alacritty is the terminal that
# will actually run: it tells thegn's font picker which file to patch, and
# Alacritty is 4th of 5 candidates here. Exported unconditionally, a bundle that
# resolved to Ghostty still pointed the picker at the Alacritty profile — so
# "Switch font" edited a file nothing was reading and reported success.
thegn_command() {
  local bin="$1" term="${2:-}"
  printf 'exec %s %s %s %s' \
    "$(printf '%q' "${BASH:-/bin/bash}")" \
    "$(printf '%q' "$THEGN_RES/launch.sh")" \
    "$(printf '%q' "$bin")" \
    "$(printf '%q' "$term")"
}
EOF
} >"$staging/Contents/Resources/common.sh"
chmod 0644 "$staging/Contents/Resources/common.sh"

: >"$staging/Contents/Resources/env"
for kv in ${envs+"${envs[@]}"}; do
  printf '%s\0' "$kv" >>"$staging/Contents/Resources/env"
done
chmod 0644 "$staging/Contents/Resources/env"

# ── Contents/Resources/launch.sh — static environment loader ────────────────
cat >"$staging/Contents/Resources/launch.sh" <<'EOF'
#!/bin/bash
# Generated by packaging/macos/make-app.sh — do not edit; re-run the generator.
set -uo pipefail
# shellcheck source=/dev/null
. "$(cd -- "$(dirname -- "$0")" && pwd)/common.sh"
bin="${1:?}"
term="${2:-}"
load_env || exit 1
if [ -n "$THEGN_ALACRITTY_CFG" ] && [ "$(basename "$term")" = alacritty ]; then
  export THEGN_ALACRITTY_CONFIG="$THEGN_ALACRITTY_CFG"
fi
exec "$bin"
EOF
chmod 0755 "$staging/Contents/Resources/launch.sh"

# ── Contents/MacOS/<name> — the bundle executable ────────────────────────────
cat >"$staging/Contents/MacOS/$name" <<EOF
#!/bin/bash
# Generated by packaging/macos/make-app.sh — do not edit; re-run the generator.
set -uo pipefail
RES="\$(cd -- "\$(dirname -- "\$0")/../Resources" && pwd)"
# shellcheck source=/dev/null
. "\$RES/common.sh"
TERM_PREF=$(q "$terminal")
EOF
cat >>"$staging/Contents/MacOS/$name" <<'EOF'

bin="$(resolve_bin)" || {
  alert "thegn binary not found. Re-run ./install.sh (or 'just macos-app') to point this app at it."
  exit 1
}
# Terminal emulators, most-preferred first. Bundle paths are checked directly
# because a Mac terminal's CLI is usually NOT on PATH — Ghostty, kitty, WezTerm
# and Alacritty all ship as .app bundles whose binary lives inside.
candidates=(
  "$TERM_PREF"
  "$HOME/Applications/Ghostty.app/Contents/MacOS/ghostty"
  "/Applications/Ghostty.app/Contents/MacOS/ghostty"
  "$HOME/Applications/WezTerm.app/Contents/MacOS/wezterm"
  "/Applications/WezTerm.app/Contents/MacOS/wezterm"
  "$HOME/Applications/kitty.app/Contents/MacOS/kitty"
  "/Applications/kitty.app/Contents/MacOS/kitty"
  "$HOME/Applications/Alacritty.app/Contents/MacOS/alacritty"
  "/Applications/Alacritty.app/Contents/MacOS/alacritty"
)
# Nix/Homebrew installs put real CLI binaries in the same prefixes thegn lives in.
for d in "${THEGN_DIRS[@]}"; do
  for t in ghostty wezterm kitty alacritty; do
    candidates+=("$d/$t")
  done
done

term=""
for c in "${candidates[@]}"; do
  if [ -n "$c" ] && [ -x "$c" ]; then
    term="$c"
    break
  fi
done

# Built AFTER the terminal is resolved so the launch script receives its path.
cmd="$(thegn_command "$bin" "$term")"

if [ -n "$term" ]; then
  case "$(basename "$term")" in
  ghostty) exec "$term" -e "$THEGN_LOGIN_SHELL" -l -c "$cmd" ;;
  wezterm) exec "$term" start -- "$THEGN_LOGIN_SHELL" -l -c "$cmd" ;;
  kitty) exec "$term" "$THEGN_LOGIN_SHELL" -l -c "$cmd" ;;
  alacritty)
    if [ -n "$THEGN_ALACRITTY_CFG" ] && [ -f "$THEGN_ALACRITTY_CFG" ]; then
      exec "$term" --config-file "$THEGN_ALACRITTY_CFG" -e "$THEGN_LOGIN_SHELL" -l -c "$cmd"
    fi
    exec "$term" -e "$THEGN_LOGIN_SHELL" -l -c "$cmd"
    ;;
  esac
fi

# Last resort: Terminal.app, which is on every Mac. It runs a `.command` file in
# a login shell of its own, so the runner just execs the binary.
exec /usr/bin/open -a Terminal "$RES/$APP_NAME.command"
EOF
chmod 0755 "$staging/Contents/MacOS/$name"

# ── Contents/Resources/<name>.command — the Terminal.app runner ──────────────
cat >"$staging/Contents/Resources/$name.command" <<'EOF'
#!/bin/bash
# Generated by packaging/macos/make-app.sh — do not edit; re-run the generator.
# Terminal.app opens .command files in a login shell, so no re-exec is needed.
set -uo pipefail
RES="$(cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=/dev/null
. "$RES/common.sh"
bin="$(resolve_bin)" || {
  alert "thegn binary not found. Re-run ./install.sh (or 'just macos-app')."
  exit 1
}
exec "${BASH:-/bin/bash}" "$RES/launch.sh" "$bin" ""
EOF
chmod 0755 "$staging/Contents/Resources/$name.command"

# ── Icon ─────────────────────────────────────────────────────────────────────
if [[ -f $icon ]]; then
  cp "$icon" "$staging/Contents/Resources/$name.icns"
else
  echo "make-app.sh: warning: $icon missing — the app will use a generic icon" \
    "(regenerate with 'python3 scripts/gen-owl-icns.py')" >&2
fi

# ── Info.plist ───────────────────────────────────────────────────────────────
cat >"$staging/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleDisplayName</key><string>$name</string>
  <key>CFBundleExecutable</key><string>$name</string>
  <key>CFBundleIdentifier</key><string>com.github.blakeashleyjr.$name</string>
  <key>CFBundleIconFile</key><string>$name</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$short_version</string>
  <key>CFBundleVersion</key><string>$build_version</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <!-- Deliberately NOT LSUIElement: the launcher execs into the terminal
       emulator, so LaunchServices ends up tracking the terminal and no
       windowless thegn process lingers in the Dock. Marking it an agent would
       buy nothing, and some launchers (Raycast, the app switcher) hide them. -->
</dict>
</plist>
EOF
printf 'APPL????' >"$staging/Contents/PkgInfo"
printf 'thegn make-app 1\n' >"$staging/Contents/Resources/.thegn-make-app"

if command -v plutil >/dev/null 2>&1; then
  if ! plutil -lint "$staging/Contents/Info.plist"; then
    echo "make-app.sh: plist validation failed: $staging/Contents/Info.plist" >&2
    exit 1
  fi
fi

chmod 0755 "$staging"
if [[ -e $app ]]; then
  old="$(mktemp -d "$dest/.$name.app.old.XXXXXXXX")"
  mv "$app" "$old/app"
  if ! mv "$staging" "$app"; then
    if mv "$old/app" "$app"; then
      rmdir "$old"
      old=""
    else
      echo "make-app.sh: failed to promote staged bundle and failed to restore $app; previous bundle is at $old/app" >&2
    fi
    exit 1
  fi
else
  mv "$staging" "$app"
fi
staging=""
if [[ -n $old ]]; then
  rm -rf -- "$old"
  old=""
fi

# Bump the bundle mtime and re-register so Spotlight/Raycast/the Dock pick up a
# re-generated bundle immediately instead of serving a stale cached copy.
touch "$app"
lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[[ -x $lsregister ]] && "$lsregister" -f "$app" >/dev/null 2>&1 || true

echo "wrote app bundle: $app"
echo "  launches: $bin (in a terminal window; Spotlight/Raycast/Dock searchable as '$name')"
