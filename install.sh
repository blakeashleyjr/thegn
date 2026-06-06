#!/usr/bin/env bash
# install.sh — standalone (non-Nix) install: build a release binary and symlink
# it (plus the layouts) into place. The Nix/home-manager path does all of this
# declaratively; this is for a quick local setup.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bindir="${1:-$HOME/.local/bin}"
: "${XDG_CONFIG_HOME:=$HOME/.config}"
layoutdir="$XDG_CONFIG_HOME/zellij/layouts"
# Literal ~/.local/share to match the `file:~/.local/share/...` plugin paths in
# the session layout (and thus zellij's permission-cache keys).
datadir="$HOME/.local/share/superzej"

command -v cargo >/dev/null || {
  echo "cargo not found — install Rust or use 'nix profile install'." >&2
  exit 1
}

echo "building release binary…"
(cd "$here" && cargo build --release)

echo "building WASM plugins (wasm32-wasip1)…"
rustup target add wasm32-wasip1 2>/dev/null || true
(cd "$here/plugin/sidebar" && cargo build --release --target wasm32-wasip1)
(cd "$here/plugin/panel" && cargo build --release --target wasm32-wasip1)
(cd "$here/plugin/tabbar" && cargo build --release --target wasm32-wasip1)
(cd "$here/plugin/statusbar" && cargo build --release --target wasm32-wasip1)

mkdir -p "$bindir" "$layoutdir" "$XDG_CONFIG_HOME/superzej" "$datadir"

ln -sfn "$here/target/release/superzej" "$bindir/superzej"
ln -sfn "$here/target/release/superzej" "$bindir/sj"
ln -sfn "$here/layouts/superzej.kdl" "$layoutdir/superzej.kdl"
ln -sfn "$here/layouts/worktree-tab.kdl" "$layoutdir/worktree-tab.kdl"
ln -sfn "$here/layouts/home-tab.kdl" "$layoutdir/home-tab.kdl"
ln -sfn "$here/layouts/worktree-tab-extra.kdl" "$layoutdir/worktree-tab-extra.kdl"
ln -sfn "$here/layouts/worktree-tab-restore.kdl" "$layoutdir/worktree-tab-restore.kdl"
ln -sfn "$here/plugin/sidebar/target/wasm32-wasip1/release/superzej-sidebar.wasm" "$datadir/sidebar.wasm"
ln -sfn "$here/plugin/panel/target/wasm32-wasip1/release/superzej-panel.wasm" "$datadir/panel.wasm"
ln -sfn "$here/plugin/tabbar/target/wasm32-wasip1/release/superzej-tabbar.wasm" "$datadir/tabbar.wasm"
ln -sfn "$here/plugin/statusbar/target/wasm32-wasip1/release/superzej-statusbar.wasm" "$datadir/statusbar.wasm"

if [[ ! -f "$XDG_CONFIG_HOME/superzej/config.toml" ]]; then
  cp "$here/config/config.toml.example" "$XDG_CONFIG_HOME/superzej/config.toml"
  echo "wrote default config: $XDG_CONFIG_HOME/superzej/config.toml"
fi

# Seed the superzej-managed zellij config (customize it freely; never overwritten).
mkdir -p "$HOME/.superzej"
if [[ ! -f "$HOME/.superzej/zellij.kdl" ]]; then
  cp "$here/config/zellij.kdl" "$HOME/.superzej/zellij.kdl"
  echo "wrote managed zellij config: $HOME/.superzej/zellij.kdl"
fi

# Pre-grant the plugins' zellij permissions so the first session doesn't prompt.
"$here/target/release/superzej" grant-plugins || true

# Warn about missing runtime deps (delta is required for diff output).
command -v delta >/dev/null || echo "warning: 'delta' not found — diff output will lack syntax highlighting (install: https://github.com/dandavison/delta)" >&2

echo "installed:"
echo "  $bindir/{superzej,sj} -> $here/target/release/superzej"
echo "  $layoutdir/{superzej,worktree-tab}.kdl"
echo "  $datadir/{sidebar,panel,tabbar,statusbar}.wasm"
echo "  $HOME/.superzej/zellij.kdl  (managed zellij config — customize here)"
echo
echo "Ensure $bindir is on PATH, then run:  sj"
echo "superzej shells out to:  git zellij fzf (or gum) lazygit yazi"
echo "runtime deps (required):  delta  (install: https://github.com/dandavison/delta)"
echo
echo "Nix users: 'nix profile install $here#default' bundles everything."
