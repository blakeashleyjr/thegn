#!/usr/bin/env bash
# install.sh — standalone (non-Nix) install. By default this installs the native
# compositor host (`szhost`) as `superzej`/`sj`; pass `--zellij` for the legacy
# zellij/WASM launcher while the native host is reaching parity.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: ./install.sh [--native|--zellij] [--dry-run] [bindir]

  --native   install the native host (default): superzej/sj/szhost -> target/release/szhost
  --zellij   install the legacy zellij/WASM launcher: superzej/sj -> target/release/superzej
  --dry-run  print the install plan without building or changing files

bindir defaults to ~/.local/bin.
EOF
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mode="${SUPERZEJ_INSTALL_MODE:-native}"
dry_run=0
bindir=""

while (($#)); do
  case "$1" in
  --native | native)
    mode="native"
    ;;
  --zellij | zellij | --legacy | legacy)
    mode="zellij"
    ;;
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

case "$mode" in
native | zellij) ;;
*)
  echo "unknown SUPERZEJ_INSTALL_MODE: $mode" >&2
  usage
  exit 2
  ;;
esac

bindir="${bindir:-$HOME/.local/bin}"
: "${XDG_CONFIG_HOME:=$HOME/.config}"
# Literal ~/.local/share to match the `file:~/.local/share/...` plugin paths in
# the legacy zellij session layout (and thus zellij's permission-cache keys).
datadir="$HOME/.local/share/superzej"

if ((dry_run)); then
  echo "dry-run: no files will be changed"
else
  command -v cargo >/dev/null || {
    echo "cargo not found — install Rust or use 'nix profile install'." >&2
    exit 1
  }

  echo "building release binaries…"
  (cd "$here" && cargo build --release)
fi

echo "mode: $mode"

install_native() {
  if ((dry_run)); then
    echo "$bindir/superzej -> $here/target/release/szhost"
    echo "$bindir/sj -> $here/target/release/szhost"
    echo "$bindir/szhost -> $here/target/release/szhost"
    echo "$bindir/superzej-cli -> $here/target/release/superzej"
    echo "legacy WASM plugin symlinks under $datadir will be removed"
    return
  fi

  mkdir -p "$bindir"
  ln -sfn "$here/target/release/szhost" "$bindir/superzej"
  ln -sfn "$here/target/release/szhost" "$bindir/sj"
  ln -sfn "$here/target/release/szhost" "$bindir/szhost"
  # Keep the transitional zellij-driven CLI available under an explicit name for
  # old subcommands while the native host reaches parity.
  ln -sfn "$here/target/release/superzej" "$bindir/superzej-cli"

  for plugin in sidebar panel tabbar statusbar; do
    legacy="$datadir/$plugin.wasm"
    if [[ -L $legacy ]]; then
      unlink "$legacy"
    fi
  done

  echo "installed:"
  echo "  $bindir/{superzej,sj,szhost} -> $here/target/release/szhost"
  echo "  $bindir/superzej-cli -> $here/target/release/superzej (legacy zellij/WASM CLI)"
  echo
  echo "Ensure $bindir is on PATH, then run:  sj"
  echo "native host binary:  szhost"
}

install_zellij() {
  if ((dry_run)); then
    echo "$bindir/superzej -> $here/target/release/superzej"
    echo "$bindir/sj -> $here/target/release/superzej"
    echo "$datadir/{sidebar,panel,tabbar,statusbar}.wasm"
    echo "$HOME/.superzej/zellij.kdl"
    return
  fi

  echo "building WASM plugins (wasm32-wasip1)…"
  rustup target add wasm32-wasip1 2>/dev/null || true
  (cd "$here/plugin/sidebar" && cargo build --release --target wasm32-wasip1)
  (cd "$here/plugin/panel" && cargo build --release --target wasm32-wasip1)
  (cd "$here/plugin/tabbar" && cargo build --release --target wasm32-wasip1)
  (cd "$here/plugin/statusbar" && cargo build --release --target wasm32-wasip1)

  mkdir -p "$bindir" "$XDG_CONFIG_HOME/superzej" "$datadir"

  ln -sfn "$here/target/release/superzej" "$bindir/superzej"
  ln -sfn "$here/target/release/superzej" "$bindir/sj"
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
  echo "  $datadir/{sidebar,panel,tabbar,statusbar}.wasm"
  echo "  layouts seeded to ~/.superzej/layouts on first launch"
  echo "  $HOME/.superzej/zellij.kdl  (managed zellij config — customize here)"
  echo
  echo "Ensure $bindir is on PATH, then run:  sj"
  echo "superzej shells out to:  git zellij fzf (or gum) lazygit yazi"
  echo "runtime deps (required):  delta  (install: https://github.com/dandavison/delta)"
}

case "$mode" in
native) install_native ;;
zellij) install_zellij ;;
esac

echo
echo "Nix users: 'nix profile install $here#default' bundles the native host."
