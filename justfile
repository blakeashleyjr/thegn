# superzej — dev & build tasks. Run `just` to list, `just <recipe>` to run.
# Recipes assume the dev shell (`nix develop`) or the deps on PATH.

bin := "target/debug/superzej"

# Show available recipes (default).
default:
    @just --list

# --- build / package ------------------------------------------------------

# Debug build.
build:
    cargo build

# Release build.
release:
    cargo build --release

# Build the WASM zellij plugins (sidebar + panel + tabbar + statusbar) -> plugin/*/target/wasm32-wasip1.
build-plugins:
    rustup target add wasm32-wasip1 2>/dev/null || true
    cd plugin/sidebar && cargo build --release --target wasm32-wasip1
    cd plugin/panel && cargo build --release --target wasm32-wasip1
    cd plugin/tabbar && cargo build --release --target wasm32-wasip1
    cd plugin/statusbar && cargo build --release --target wasm32-wasip1


# Build the Nix package; symlinks ./result.
nix-build:
    nix build .#default

# Build the Nix plugin packages.
nix-build-plugins:
    nix build .#superzej-sidebar .#superzej-panel .#superzej-tabbar .#superzej-statusbar

# Print the store path without creating ./result.
path:
    @nix build .#default --no-link --print-out-paths

# Evaluate all flake outputs.
flake-check:
    nix flake check

# The full pre-commit gate.
ci: fmt-check lint build build-plugins test smoke nix-build
    @echo "ci: all green"

# --- quality --------------------------------------------------------------

# Comprehensive linting: rust (clippy), bash (shellcheck), yaml (yamllint), toml (taplo).
lint: check-theme
    cargo clippy --all-targets -- -D warnings
    shellcheck -x install.sh test/smoke.sh
    yamllint .
    taplo lint

# Copy the canonical theme (src/theme.rs) into each plugin crate. The plugins
# can't share a crate (the Nix builds sandbox each plugin subdir), so they
# carry committed copies; this keeps them in sync.
sync-theme:
    for p in sidebar tabbar panel statusbar; do \
      cp src/theme.rs plugin/$p/src/theme.rs; done

# Fail if any plugin's theme.rs has drifted from the canonical src/theme.rs.
check-theme:
    @for p in sidebar tabbar panel statusbar; do \
      diff -q src/theme.rs plugin/$p/src/theme.rs \
        || { echo "theme drift in plugin/$p — run 'just sync-theme'"; exit 1; }; done

# Format everything via treefmt (rust, nix, bash, toml, yaml, markdown).
fmt:
    nix fmt

# Check formatting without writing (CI-friendly).
fmt-check:
    nix fmt -- --ci

# Unit tests.
test:
    cargo test

# Hermetic end-to-end test against the debug binary (no zellij side effects).
smoke: build
    ./test/smoke.sh {{bin}}

# Same, but against the built Nix package (verifies the wrapper + injected deps).
smoke-pkg:
    ./test/smoke.sh "$(nix build .#default --no-link --print-out-paths)/bin/superzej"

# End-to-end checks. one-session: superzej is ONE session and every repo is a
# tab, so opening/selecting never creates or switches a session (needs zellij).
# slug-unique: same-basename repos get distinct tab namespaces (hermetic).
e2e: release
    ./test/one-session.sh
    ./test/slug-unique.sh

# --- run / install --------------------------------------------------------

# Run a subcommand against the debug build, e.g. `just run list --json`.
run *args: build
    {{bin}} {{args}}

# Build and run superzej locally in a FULLY ISOLATED instance — its own data
# root `~/.superzej-{{name}}` (config, worktrees, zellij socket/cache) and DB
# (XDG_STATE_HOME under it), and the pinned zellij. So it never touches your
# daily-driver superzej. `just start work2` is a second, independent instance.
# Run from a NON-zellij terminal (zellij can't nest). Rebuilds binary + plugins.
start name="dev": build build-plugins
    mkdir -p "$HOME/.superzej-{{name}}/state"
    PATH="$PWD/target/debug:$PATH" \
      SUPERZEJ_ZELLIJ_BIN="$(nix build --no-link --print-out-paths .#zellij)/bin/zellij" \
      SUPERZEJ_DIR="$HOME/.superzej-{{name}}" \
      XDG_STATE_HOME="$HOME/.superzej-{{name}}/state" \
      SUPERZEJ_LAYOUT="$PWD/layouts/superzej.kdl" \
      SUPERZEJ_CONFIG="$PWD/config/zellij.kdl" \
      {{bin}}

# Alias for `start`.
attach: start

# Build and open a FULLY ISOLATED superzej in a fresh ghostty window. Same
# isolation as `start` (own `~/.superzej-{{name}}` root + DB + pinned zellij),
# so you can develop here without touching your daily-driver superzej; pass a
# name for independent parallel instances (`just start-term work2`).
# `ghostty -e` runs the binary DIRECTLY (no fish autostart, so no nesting).
# SUPERZEJ_FRESH force-kills this instance's same-named session first, so every
# launch is a clean session on the latest layout/config/theme. The cache wipe
# targets this instance's OWN cache only — never the system ~/.cache/zellij.
start-term name="dev": build build-plugins
    mkdir -p "$HOME/.superzej-{{name}}/state"
    -find "$HOME/.superzej-{{name}}/cache" -type d -name '*.wasm' -prune -exec rm -rf {} + 2>/dev/null
    setsid -f ghostty -e env \
      -u ZELLIJ -u ZELLIJ_SESSION_NAME -u ZELLIJ_PANE_ID \
      "PATH=$PWD/target/debug:$PATH" \
      "SUPERZEJ_ZELLIJ_BIN=$(nix build --no-link --print-out-paths .#zellij)/bin/zellij" \
      "SUPERZEJ_DIR=$HOME/.superzej-{{name}}" \
      "XDG_STATE_HOME=$HOME/.superzej-{{name}}/state" \
      "SUPERZEJ_LAYOUT=$PWD/layouts/superzej.kdl" \
      "SUPERZEJ_CONFIG=$PWD/config/zellij.kdl" \
      "SUPERZEJ_FRESH=1" \
      "$PWD/target/debug/superzej"

# Install/update the latest superzej onto your PATH (standalone, non-Nix):
# builds a release binary + WASM plugins and symlinks `superzej`/`sj`, the
# layouts and plugins into place (via install.sh). Because it symlinks the
# release artifacts, re-running just rebuilds — picking up your latest changes.
# Pass a bindir to override the default (~/.local/bin), e.g. `just install ~/bin`.
install *bindir:
    ./install.sh {{bindir}}

# Enter the dev shell (default), or `just dev tui` for the auto-refreshing
# sandboxed TUI (see `dev-tui`).
dev what="shell":
    {{ if what == "tui" { "just dev-tui" } else { "nix develop" } }}

# Auto-refreshing sandboxed TUI (also reachable as `just dev tui`). Watches the
# sources and, on every save, rebuilds the binary + WASM plugins and
# force-relaunches the FULLY ISOLATED `~/.superzej-{{name}}` session (via
# `start-term`, SUPERZEJ_FRESH=1) so the running TUI always reflects the latest
# build. Runs once immediately; Ctrl-C stops the watcher. Never touches your
# daily-driver superzej. Run from a NON-zellij terminal (zellij can't nest).
# The watch set is scoped to source dirs, so build outputs don't retrigger it.
dev-tui name="dev":
    cargo watch -w src -w plugin -w layouts -w config -s "just start-term {{name}}"

# Remove build artifacts.
clean:
    cargo clean
    rm -f result result-*
