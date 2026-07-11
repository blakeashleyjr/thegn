#!/usr/bin/env bash
# install.sh — standalone (non-Nix) install of the native compositor host.
#
# Installs:
#   tg      — opens thegn in a dedicated alacritty window with the bundled profile
#   tg-tui  — opens thegn in the current terminal window
#   thegn   — direct native host binary for CLI verbs/current-terminal use
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

release_bin="$here/target/release/thegn"
alacritty_config="$here/config/alacritty.toml"
tg_tui="$bindir/tg-tui"

if ((dry_run)); then
  echo "dry-run: no files will be changed"
  echo "$bindir/thegn -> $release_bin"
  echo "$bindir/tg-tui wrapper -> $release_bin (current terminal)"
  echo "$bindir/tg wrapper -> alacritty --config-file $alacritty_config -e $tg_tui"
  exit 0
fi

command -v cargo >/dev/null || {
  echo "cargo not found — install Rust or use 'nix profile install'." >&2
  exit 1
}

echo "building release binary…"
(cd "$here" && cargo build --release --workspace)

mkdir -p "$bindir"
ln -sfn "$release_bin" "$bindir/thegn"

release_bin_q="$(shell_quote "$release_bin")"
alacritty_config_q="$(shell_quote "$alacritty_config")"
tg_tui_q="$(shell_quote "$tg_tui")"

# Remove any existing wrappers first: a leftover dangling symlink (e.g. from a
# pruned worktree) would make the heredoc redirect below fail with "No such
# file or directory" as bash follows it to a non-existent target.
# Also sweep the pre-rename superzej-era entry points.
rm -f "$tg_tui" "$bindir/tg" \
  "$bindir/sj" "$bindir/sj-tui" "$bindir/superzej" "$bindir/szhost"

cat >"$tg_tui" <<EOF
#!/usr/bin/env bash
set -euo pipefail
export THEGN_ALACRITTY_CONFIG=$alacritty_config_q
exec $release_bin_q "\$@"
EOF
chmod 0755 "$tg_tui"

cat >"$bindir/tg" <<EOF
#!/usr/bin/env bash
set -euo pipefail

if ((\$# > 0)); then
  exec $tg_tui_q "\$@"
fi

if ! command -v alacritty >/dev/null 2>&1; then
  echo "tg: alacritty not found; install alacritty or run 'tg-tui' to open thegn in the current terminal." >&2
  exit 127
fi

exec alacritty --config-file $alacritty_config_q -e $tg_tui_q
EOF
chmod 0755 "$bindir/tg"

if [[ ! -f "$XDG_CONFIG_HOME/thegn/config.toml" ]]; then
  mkdir -p "$XDG_CONFIG_HOME/thegn"
  cp "$here/config/config.toml.example" "$XDG_CONFIG_HOME/thegn/config.toml"
  echo "wrote default config: $XDG_CONFIG_HOME/thegn/config.toml"
fi

# Warn about missing runtime deps (delta is used for diff output; alacritty is
# used by the `tg` dedicated-window launcher).
command -v delta >/dev/null || echo "warning: 'delta' not found — diff output will lack syntax highlighting (install: https://github.com/dandavison/delta)" >&2
command -v alacritty >/dev/null || echo "warning: 'alacritty' not found — 'tg' opens a dedicated alacritty window; use 'tg-tui' for the current terminal" >&2

echo "installed:"
echo "  $bindir/tg      -> dedicated alacritty window using $alacritty_config"
echo "  $bindir/tg-tui  -> current-terminal native host ($release_bin)"
echo "  $bindir/thegn   -> $release_bin"
echo
echo "Ensure $bindir is on PATH, then run:  tg      # dedicated alacritty window"
echo "                              or:  tg-tui  # current terminal"
echo "thegn shells out to:  git fzf (or gum) lazygit yazi delta gh"
echo
echo "Nix users: 'nix profile install $here#default' bundles the native host."
