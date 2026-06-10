#!/usr/bin/env bash
# install.sh — standalone (non-Nix) install of the native compositor host
# (`szhost`) as `superzej`/`sj`/`szhost` on your PATH.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: ./install.sh [--dry-run] [bindir]

  --dry-run  print the install plan without building or changing files

bindir defaults to ~/.local/bin.
EOF
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

if ((dry_run)); then
  echo "dry-run: no files will be changed"
  echo "$bindir/superzej -> $here/target/release/szhost"
  echo "$bindir/sj -> $here/target/release/szhost"
  echo "$bindir/szhost -> $here/target/release/szhost"
  exit 0
fi

command -v cargo >/dev/null || {
  echo "cargo not found — install Rust or use 'nix profile install'." >&2
  exit 1
}

echo "building release binary…"
(cd "$here" && cargo build --release --workspace)

mkdir -p "$bindir"
ln -sfn "$here/target/release/szhost" "$bindir/superzej"
ln -sfn "$here/target/release/szhost" "$bindir/sj"
ln -sfn "$here/target/release/szhost" "$bindir/szhost"

if [[ ! -f "$XDG_CONFIG_HOME/superzej/config.toml" ]]; then
  mkdir -p "$XDG_CONFIG_HOME/superzej"
  cp "$here/config/config.toml.example" "$XDG_CONFIG_HOME/superzej/config.toml"
  echo "wrote default config: $XDG_CONFIG_HOME/superzej/config.toml"
fi

# Warn about missing runtime deps (delta is used for diff output).
command -v delta >/dev/null || echo "warning: 'delta' not found — diff output will lack syntax highlighting (install: https://github.com/dandavison/delta)" >&2

echo "installed:"
echo "  $bindir/{superzej,sj,szhost} -> $here/target/release/szhost"
echo
echo "Ensure $bindir is on PATH, then run:  sj"
echo "superzej shells out to:  git fzf (or gum) lazygit yazi delta gh"
echo
echo "Nix users: 'nix profile install $here#default' bundles the native host."
