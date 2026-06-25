#!/usr/bin/env bash
# install.sh — standalone (non-Nix) install of the native compositor host.
#
# Installs:
#   sj       — opens superzej in a dedicated alacritty window with the bundled profile
#   sj-tui   — opens superzej in the current terminal window
#   superzej — direct native host binary for CLI verbs/current-terminal use
#   szhost   — direct native host binary alias
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

release_bin="$here/target/release/szhost"
alacritty_config="$here/config/alacritty.toml"
sj_tui="$bindir/sj-tui"

if ((dry_run)); then
  echo "dry-run: no files will be changed"
  echo "$bindir/szhost -> $release_bin"
  echo "$bindir/superzej -> $release_bin"
  echo "$bindir/sj-tui wrapper -> $release_bin (current terminal)"
  echo "$bindir/sj wrapper -> alacritty --config-file $alacritty_config -e $sj_tui"
  exit 0
fi

command -v cargo >/dev/null || {
  echo "cargo not found — install Rust or use 'nix profile install'." >&2
  exit 1
}

echo "building release binary…"
(cd "$here" && cargo build --release --workspace)

mkdir -p "$bindir"
ln -sfn "$release_bin" "$bindir/superzej"
ln -sfn "$release_bin" "$bindir/szhost"

release_bin_q="$(shell_quote "$release_bin")"
alacritty_config_q="$(shell_quote "$alacritty_config")"
sj_tui_q="$(shell_quote "$sj_tui")"

cat >"$sj_tui" <<EOF
#!/usr/bin/env bash
set -euo pipefail
export SUPERZEJ_ALACRITTY_CONFIG=$alacritty_config_q
exec $release_bin_q "\$@"
EOF
chmod 0755 "$sj_tui"

cat >"$bindir/sj" <<EOF
#!/usr/bin/env bash
set -euo pipefail

if ((\$# > 0)); then
  exec $sj_tui_q "\$@"
fi

if ! command -v alacritty >/dev/null 2>&1; then
  echo "sj: alacritty not found; install alacritty or run 'sj-tui' to open superzej in the current terminal." >&2
  exit 127
fi

exec alacritty --config-file $alacritty_config_q -e $sj_tui_q
EOF
chmod 0755 "$bindir/sj"

if [[ ! -f "$XDG_CONFIG_HOME/superzej/config.toml" ]]; then
  mkdir -p "$XDG_CONFIG_HOME/superzej"
  cp "$here/config/config.toml.example" "$XDG_CONFIG_HOME/superzej/config.toml"
  echo "wrote default config: $XDG_CONFIG_HOME/superzej/config.toml"
fi

# Warn about missing runtime deps (delta is used for diff output; alacritty is
# used by the `sj` dedicated-window launcher).
command -v delta >/dev/null || echo "warning: 'delta' not found — diff output will lack syntax highlighting (install: https://github.com/dandavison/delta)" >&2
command -v alacritty >/dev/null || echo "warning: 'alacritty' not found — 'sj' opens a dedicated alacritty window; use 'sj-tui' for the current terminal" >&2

echo "installed:"
echo "  $bindir/sj      -> dedicated alacritty window using $alacritty_config"
echo "  $bindir/sj-tui  -> current-terminal native host ($release_bin)"
echo "  $bindir/{superzej,szhost} -> $release_bin"
echo
echo "Ensure $bindir is on PATH, then run:  sj      # dedicated alacritty window"
echo "                              or:  sj-tui  # current terminal"
echo "superzej shells out to:  git fzf (or gum) lazygit yazi delta gh"
echo
echo "Nix users: 'nix profile install $here#default' bundles the native host."
