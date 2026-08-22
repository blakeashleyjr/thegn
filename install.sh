#!/usr/bin/env bash
# install.sh — standalone (non-Nix) install of the native compositor host.
#
# Installs:
#   tg               — opens thegn in the CURRENT terminal (also forwards CLI verbs)
#   tg -s|--standalone — opens thegn in a dedicated alacritty window (bundled profile)
#   tg-tui           — always the current terminal (compat alias for `tg`)
#   thegn            — direct native host binary for CLI verbs/current-terminal use
# Plus a `.desktop` app-launcher entry (Exec=tg --standalone) with the owl icon.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: ./install.sh [--dry-run] [bindir]

  --dry-run  print the install plan without building or changing files

bindir defaults to ~/.local/bin.
EOF
}

shell_quote() {
  printf '%q' "$1"
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dry_run=0
bindir=""

while (($#)); do
  case "$1" in
  --dry-run)
    dry_run=1
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  --*)
    echo "unknown option: $1" >&2
    usage
    exit 2
    ;;
  *)
    if [[ -n $bindir ]]; then
      echo "only one bindir may be provided" >&2
      usage
      exit 2
    fi
    bindir="$1"
    ;;
  esac
  shift
done

bindir="${bindir:-$HOME/.local/bin}"
: "${XDG_CONFIG_HOME:=$HOME/.config}"
: "${XDG_DATA_HOME:=$HOME/.local/share}"

release_bin="$here/target/release/thegn"
alacritty_config="$here/config/alacritty.toml"
tg_tui="$bindir/tg-tui"
apps_dir="$XDG_DATA_HOME/applications"
desktop_file="$apps_dir/thegn.desktop"
icon_src="$here/config/thegn.svg"
icon_dir="$XDG_DATA_HOME/icons/hicolor/scalable/apps"
icon_file="$icon_dir/thegn.svg"

# Freedesktop desktop-integration (a `.desktop` launcher entry + an hicolor icon)
# is a Linux/BSD concept. Neither macOS nor Windows has an XDG launcher to
# register with, so writing those files there just litters the data dir with
# something nothing reads — the binaries and wrappers install exactly the same
# way on every platform. Opt OUT by name (rather than opting Linux in) so the
# BSDs, which do have freedesktop launchers, keep getting the entry.
desktop_integration=1
case "$(uname -s)" in
Darwin | MINGW* | MSYS* | CYGWIN*) desktop_integration=0 ;;
esac

if ((dry_run)); then
  echo "dry-run: no files will be changed"
  echo "$bindir/thegn <- copy of $release_bin"
  echo "$bindir/tg-tui wrapper -> $bindir/thegn (current terminal)"
  echo "$bindir/tg wrapper -> $tg_tui (current terminal); tg -s|--standalone -> alacritty --config-file $alacritty_config -e $tg_tui"
  if ((desktop_integration)); then
    echo "$icon_file -> $icon_src (owl app icon)"
    echo "$desktop_file -> app-launcher entry (Exec=$bindir/tg --standalone, Icon=thegn)"
  else
    echo "(no .desktop entry or hicolor icon — freedesktop launchers are Linux/BSD-only)"
  fi
  exit 0
fi

command -v cargo >/dev/null || {
  echo "cargo not found — install Rust or use 'nix profile install'." >&2
  exit 1
}

echo "building release binary…"
(cd "$here" && cargo build --release --workspace)

if [[ ! -f $release_bin ]]; then
  echo "build reported success but $release_bin is missing — nothing to install." >&2
  exit 1
fi

mkdir -p "$bindir"
# COPY, don't symlink into target/. A symlink to $here/target/release/thegn
# breaks the moment you `cargo clean`, prune the worktree, or move the repo —
# silently, and only when you next try to launch. An installed binary should
# survive the source tree it came from. Re-run install.sh to pick up a rebuild.
install -m755 "$release_bin" "$bindir/thegn"
installed_bin="$bindir/thegn"

installed_bin_q="$(shell_quote "$installed_bin")"
alacritty_config_q="$(shell_quote "$alacritty_config")"
tg_tui_q="$(shell_quote "$tg_tui")"

# Remove any existing wrappers first: a leftover dangling symlink (e.g. from a
# pruned worktree) would make the heredoc redirect below fail with "No such
# file or directory" as bash follows it to a non-existent target.
#
# Only thegn's OWN entry points are removed. The pre-rename superzej-era names
# (sj, sj-tui, superzej, szhost) used to be swept here too — but no public user
# ever had them, and `sj` in particular is a plausible name for something else
# entirely. An installer must not delete binaries it did not create.
rm -f "$tg_tui" "$bindir/tg"

# tg-tui: always the current terminal. THEGN_ALACRITTY_CONFIG points the in-app
# font picker at the alacritty profile it patches — the same bundled profile
# `tg --standalone` launches, so a font switch applies to both.
cat >"$tg_tui" <<EOF
#!/usr/bin/env bash
set -euo pipefail
export THEGN_ALACRITTY_CONFIG=$alacritty_config_q
exec $installed_bin_q "\$@"
EOF
chmod 0755 "$tg_tui"

# tg: current terminal by default; `-s`/`--standalone` (must be the first arg)
# opens a dedicated alacritty window running the bundled, hermetic profile
# (`--config-file` replaces alacritty's default config search entirely, so your
# personal alacritty.toml stays out). Any other args (CLI verbs like `tg list`)
# run in the current terminal.
cat >"$bindir/tg" <<EOF
#!/usr/bin/env bash
set -euo pipefail

if [[ \${1-} == -s || \${1-} == --standalone ]]; then
  shift
  if ! command -v alacritty >/dev/null 2>&1; then
    echo "tg: alacritty not found; install alacritty or run 'tg' to open thegn in the current terminal." >&2
    exit 127
  fi
  exec alacritty --config-file $alacritty_config_q -e $tg_tui_q "\$@"
fi

exec $tg_tui_q "\$@"
EOF
chmod 0755 "$bindir/tg"

if ((desktop_integration)); then
  # Owl app icon: the same perched-sentinel mascot the loading splash draws
  # (config/thegn.svg, generated from crates/thegn-host/src/owl.rs). Installed
  # into the user's hicolor icon theme so `Icon=thegn` resolves in any launcher.
  mkdir -p "$icon_dir"
  if [[ -f $icon_src ]]; then
    cp "$icon_src" "$icon_file"
    echo "wrote app icon: $icon_file"
  else
    echo "warning: $icon_src missing — desktop entry will fall back to a generic icon" >&2
  fi

  # App-launcher entry (GNOME/KDE/rofi/wofi/…): a `.desktop` file so thegn is
  # searchable/pinnable in your launcher. A GUI launcher has no terminal, so it
  # runs `tg --standalone` to open thegn's OWN alacritty window (`Terminal=false`).
  tg_launcher="$bindir/tg"
  mkdir -p "$apps_dir"
  cat >"$desktop_file" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=thegn
GenericName=Git Worktree IDE
Comment=Terminal-native git-worktree IDE + multiplexer
Exec=$tg_launcher --standalone
TryExec=$tg_launcher
Terminal=false
Icon=thegn
Categories=Development;IDE;RevisionControl;
Keywords=git;worktree;terminal;ide;multiplexer;thegn;
StartupNotify=true
EOF
  chmod 0644 "$desktop_file"
  # Refresh the launcher + icon caches so the entry/icon show up without a
  # re-login (best-effort — not all environments ship the tools).
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps_dir" 2>/dev/null || true
  command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t -f "$XDG_DATA_HOME/icons/hicolor" 2>/dev/null || true
  echo "wrote app-launcher entry: $desktop_file"
fi

if [[ ! -f "$XDG_CONFIG_HOME/thegn/config.toml" ]]; then
  mkdir -p "$XDG_CONFIG_HOME/thegn"
  cp "$here/config/config.toml.example" "$XDG_CONFIG_HOME/thegn/config.toml"
  echo "wrote default config: $XDG_CONFIG_HOME/thegn/config.toml"
fi

# Warn about missing runtime deps (delta is used for diff output; alacritty backs
# the `tg --standalone` dedicated-window launcher).
command -v delta >/dev/null || echo "warning: 'delta' not found — diff output will lack syntax highlighting (install: https://github.com/dandavison/delta)" >&2
command -v alacritty >/dev/null || echo "warning: 'alacritty' not found — 'tg --standalone' opens a dedicated alacritty window; plain 'tg' opens thegn in the current terminal" >&2

echo "installed:"
echo "  $bindir/tg              -> current terminal ($bindir/thegn)"
echo "  $bindir/tg --standalone -> dedicated alacritty window using $alacritty_config"
echo "  $bindir/tg-tui          -> current-terminal native host ($bindir/thegn)"
echo "  $bindir/thegn           <- copy of $release_bin (re-run install.sh after a rebuild)"
echo "  $icon_file              -> owl app icon"
echo "  $desktop_file           -> app-launcher entry ('thegn')"
echo
echo "Ensure $bindir is on PATH, then run:  tg              # current terminal"
echo "                              or:  tg --standalone  # dedicated alacritty window"
echo "thegn shells out to:  git fzf (or gum) lazygit yazi delta gh"
echo
echo "Nix users: 'nix profile install $here#default' bundles the native host."
