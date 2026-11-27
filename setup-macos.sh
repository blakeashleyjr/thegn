#!/usr/bin/env bash
# setup-macos.sh — one-command dev setup for macOS contributors.
#
# Checks each prerequisite and offers to install what's missing. Nothing is
# installed without asking (pass --yes to accept every offer). Two paths:
#   Nix present  → point at `nix develop` (the recommended, CI-tested shell)
#   no Nix       → rustup + Homebrew deps for a bare `cargo build`
#
# See CONTRIBUTING.md for the full onboarding guide.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/bootstrap-artifacts.sh
source "$here/scripts/lib/bootstrap-artifacts.sh"

# ── Reviewed bootstrap identities ───────────────────────────────────────────
# The bare path installs rustup from a pinned `rustup-init` binary, never by
# piping the current remote script into a shell. Provenance: the official
# per-version checksum published at
#   https://static.rust-lang.org/rustup/archive/<version>/<target>/rustup-init.sha256
# re-hashed here on download. Refresh version, sizes, digests, and caps together
# as one reviewed edit; see docs/self-hosted-ci.md for the procedure.
RUSTUP_INIT_VERSION="1.29.3"
RUSTUP_INIT_SHA256_AARCH64="63752a1919a3a4793173ec70f8f212621a7ce38ba6a246095984482f4de20f9f"
RUSTUP_INIT_SIZE_AARCH64=11408656
RUSTUP_INIT_SHA256_X86_64="bc107f1fb0228de7fa32413c89c1b305cea9cd256e94b01150cd054128145429"
RUSTUP_INIT_SIZE_X86_64=13041240
RUSTUP_INIT_MAX_BYTES=16777216
RUSTUP_INIT_HOSTS="static.rust-lang.org"

die() {
  printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2
  return 1
}

rustup_init_target() {
  case "$(uname -m)" in
  arm64 | aarch64) printf '%s\n' aarch64-apple-darwin ;;
  x86_64) printf '%s\n' x86_64-apple-darwin ;;
  *) return 1 ;;
  esac
}

# Download, authenticate, then run the pinned rustup bootstrap. The resolved
# version comes from the pinned URL, so "latest" cannot change what executes.
install_rustup_pinned() {
  local target="" url sha256="" size="" staging installer status=0
  if ! target="$(rustup_init_target)"; then
    die "unsupported macOS architecture '$(uname -m)'; install rustup from https://rustup.rs"
    return 1
  fi
  case "$target" in
  aarch64-apple-darwin)
    sha256="$RUSTUP_INIT_SHA256_AARCH64"
    size="$RUSTUP_INIT_SIZE_AARCH64"
    ;;
  x86_64-apple-darwin)
    sha256="$RUSTUP_INIT_SHA256_X86_64"
    size="$RUSTUP_INIT_SIZE_X86_64"
    ;;
  esac
  url="https://static.rust-lang.org/rustup/archive/$RUSTUP_INIT_VERSION/$target/rustup-init"
  if ! staging="$(bootstrap_staging_dir)"; then
    die 'cannot create a private staging directory for rustup-init'
    return 1
  fi
  installer="$staging/rustup-init"
  if bootstrap_download_pinned "$url" "$sha256" "$size" "$RUSTUP_INIT_MAX_BYTES" \
    180 "$installer" 'rustup-init' "$RUSTUP_INIT_HOSTS"; then
    if chmod 0700 "$installer"; then
      "$installer" -y --default-toolchain stable || status=$?
    else
      die 'cannot mark the authenticated rustup-init executable'
      status=1
    fi
  else
    status=1
  fi
  rm -rf -- "$staging"
  return "$status"
}

# Sourced by test/setup-macos-test.sh to exercise the pins and every refusal
# without the interactive setup, a network call, or sudo.
if [ "${THEGN_SETUP_MACOS_SOURCE_ONLY:-0}" = 1 ]; then
  # shellcheck disable=SC2317 # reached when this file is executed, not sourced
  return 0 2>/dev/null || exit 0
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "setup-macos.sh is for macOS. On Linux: install Nix, then 'nix develop'." >&2
  exit 1
fi

yes_all=0
case "${1:-}" in
--yes | -y) yes_all=1 ;;
-h | --help)
  echo "usage: ./setup-macos.sh [--yes]"
  exit 0
  ;;
"") ;;
*)
  echo "unknown argument: $1 (usage: ./setup-macos.sh [--yes])" >&2
  exit 1
  ;;
esac

cd "$here" || exit 1

ok() { printf '  \033[32mok\033[0m    %s\n' "$1"; }
miss() { printf '  \033[33mMISS\033[0m  %s\n' "$1"; }
step() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# ask "<question>" → 0 if the user (or --yes) accepts.
ask() {
  if ((yes_all)); then
    echo "  $1 — yes (--yes)"
    return 0
  fi
  local reply
  read -r -p "  $1 [y/N] " reply
  [[ $reply == [yY]* ]]
}

step "thegn macOS setup"

# ── Xcode Command Line Tools (compiler + git; needed on every path) ─────────
if xcode-select -p >/dev/null 2>&1; then
  ok "Xcode Command Line Tools"
else
  miss "Xcode Command Line Tools (compiler + git)"
  if ask "run 'xcode-select --install' now?"; then
    xcode-select --install || true
    echo "  Finish the installer dialog, then re-run ./setup-macos.sh"
    exit 0
  fi
  echo "  Cannot build without a compiler; aborting." >&2
  exit 1
fi

# ── Path 1: Nix (recommended) ────────────────────────────────────────────────
if command -v nix >/dev/null 2>&1; then
  ok "Nix — using the recommended dev-shell path"
  if command -v direnv >/dev/null 2>&1 && [[ -f .envrc ]]; then
    if ask "enable direnv for this repo (auto-enters the dev shell)?"; then
      direnv allow
    fi
  fi
  step "Next steps"
  cat <<'EOF'
  nix develop          # toolchain + all dev tools (first run downloads a lot)
  just build           # debug build
  just start name=dev  # run the compositor with isolated state
EOF
  exit 0
fi

miss "Nix (recommended — pinned toolchain, all dev tools, CI parity)"
echo "  Install: https://install.determinate.systems (then re-run this script)"
if ! ask "continue WITHOUT Nix (rustup + Homebrew)?"; then
  exit 0
fi

# ── Path 2: bare macOS (rustup + Homebrew) ───────────────────────────────────
step "Bare-macOS prerequisites"

if command -v cargo >/dev/null 2>&1; then
  ok "rust toolchain ($(rustc --version 2>/dev/null || echo cargo))"
else
  miss "rust toolchain (stable ≥ 1.96)"
  if ask "install rustup (pinned rustup-init $RUSTUP_INIT_VERSION, digest-verified, no sudo)?"; then
    if ! install_rustup_pinned; then
      echo "  rustup installation failed; install it yourself from https://rustup.rs" >&2
      exit 1
    fi
    # shellcheck disable=SC1091 # generated by rustup above
    [[ -f "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"
  else
    echo "  Cannot build without cargo; aborting." >&2
    exit 1
  fi
fi

if command -v brew >/dev/null 2>&1; then
  ok "Homebrew"
  for pkg in pkg-config just; do
    if command -v "$pkg" >/dev/null 2>&1; then
      ok "$pkg"
    else
      miss "$pkg"
      if ask "brew install $pkg?"; then brew install "$pkg"; fi
    fi
  done
  # Optional runtime tools thegn shells out to when present.
  optional=(gh lazygit fzf gum git-delta yazi)
  wanted=()
  for pkg in "${optional[@]}"; do
    bin="$pkg"
    [[ $pkg == git-delta ]] && bin="delta"
    command -v "$bin" >/dev/null 2>&1 || wanted+=("$pkg")
  done
  if ((${#wanted[@]})); then
    if ask "install optional tools (${wanted[*]}) — PR panels, lazygit, file drawer?"; then
      brew install "${wanted[@]}"
    fi
  else
    ok "optional tools (gh lazygit fzf gum delta yazi)"
  fi
else
  miss "Homebrew — install pkg-config + just yourself, or get brew: https://brew.sh"
fi

# ── Build ─────────────────────────────────────────────────────────────────────
step "Build"
if ask "build now and install tg / tg-tui / thegn to ~/.local/bin (./install.sh)?"; then
  ./install.sh
else
  cat <<'EOF'
  Dev loop (no install):
    cargo build --workspace     # or: just build
    just start name=dev         # run the compositor with isolated state
  When you want real binaries on PATH later: ./install.sh
EOF
fi

step "Done — see CONTRIBUTING.md for the dev loop and macOS notes."
