#!/usr/bin/env bash
# install.sh — standalone (non-Nix) install: build a release binary and symlink
# it (plus the layouts) into place. The Nix/home-manager path does all of this
# declaratively; this is for a quick local setup.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bindir="${1:-$HOME/.local/bin}"
: "${XDG_CONFIG_HOME:=$HOME/.config}"
layoutdir="$XDG_CONFIG_HOME/zellij/layouts"

command -v cargo >/dev/null || {
  echo "cargo not found — install Rust or use 'nix profile install'." >&2
  exit 1
}

echo "building release binary…"
(cd "$here" && cargo build --release)

mkdir -p "$bindir" "$layoutdir" "$XDG_CONFIG_HOME/superzej"

ln -sfn "$here/target/release/superzej" "$bindir/superzej"
ln -sfn "$here/target/release/superzej" "$bindir/sj"
ln -sfn "$here/layouts/superzej.kdl" "$layoutdir/superzej.kdl"
ln -sfn "$here/layouts/workspace-tab.kdl" "$layoutdir/workspace-tab.kdl"

if [[ ! -f "$XDG_CONFIG_HOME/superzej/config.toml" ]]; then
  cp "$here/config/config.toml.example" "$XDG_CONFIG_HOME/superzej/config.toml"
  echo "wrote default config: $XDG_CONFIG_HOME/superzej/config.toml"
fi

echo "installed:"
echo "  $bindir/{superzej,sj} -> $here/target/release/superzej"
echo "  $layoutdir/{superzej,workspace-tab}.kdl"
echo
echo "Ensure $bindir is on PATH, then run:  sj"
echo "superzej shells out to: git zellij fzf (or gum); optional: lazygit yazi delta"
echo
echo "Nix users: 'nix profile install $here#default' gives a fully-wrapped binary."
