# superzej — dev & build tasks. Run `just` to list, `just <recipe>` to run.
# Recipes assume the dev shell (`nix develop`) or the deps on PATH.

# The transitional zellij-driven shell (crate `superzej-cli`).
bin := "target/debug/superzej"
# The native compositor host (crate `superzej-host`); becomes `superzej` at parity.
host_bin := "target/debug/szhost"
# Canonical theme palette, copied into each WASM plugin by `sync-theme`.
theme_src := "crates/superzej-core/src/theme.rs"

# Show available recipes (default).
default:
    @just --list

# --- build / package ------------------------------------------------------

# Debug build (the whole cargo workspace: core, cli, svc, host).
build:
    cargo build --workspace

# Release build (the whole cargo workspace).
release:
    cargo build --workspace --release

# Run the native host compositor (szhost). Builds it first. Run from a real
# terminal — it acquires raw mode and owns the screen.
host *args: build
    {{host_bin}} {{args}}

# Build the WASM zellij plugins (sidebar + panel + tabbar + statusbar) -> plugin/*/target/wasm32-wasip1.
# The plugins compile for wasm32-wasip1. That target lives in the FLAKE dev shell's
# toolchain (and `just nix-build-plugins`); the devenv/nixpkgs dev shell deliberately
# omits it (to keep rustfmt/clippy matching treefmt), and a plain shell may not have
# it. So: build with the ambient `cargo` when it already supports the target (inside
# `nix develop`, or a rustup toolchain with it added); otherwise build THROUGH the
# flake, so this works from the devenv shell or a plain shell too — instead of the
# cryptic "can't find crate for core" you get when the ambient toolchain lacks it.
build-plugins:
    #!/usr/bin/env bash
    set -euo pipefail
    plugins=(sidebar panel tabbar statusbar)
    build() { for p in "${plugins[@]}"; do ( cd "plugin/$p" && cargo build --release --target wasm32-wasip1 ); done; }
    # True when the toolchain `cargo` will use already has wasm32-wasip1 std.
    have_wasm() { local s; s="$(rustc --print sysroot 2>/dev/null)" || return 1; compgen -G "$s/lib/rustlib/wasm32-wasip1/lib/*.rlib" >/dev/null 2>&1; }

    if have_wasm; then
        build
    elif command -v rustup >/dev/null 2>&1 && rustup target add wasm32-wasip1 >/dev/null 2>&1 && have_wasm; then
        build
    else
        echo "build-plugins: ambient toolchain has no wasm32-wasip1 target — building via the flake dev shell (run 'just dev' first for a faster loop)…" >&2
        nix develop --command bash -c 'set -euo pipefail; for p in sidebar panel tabbar statusbar; do ( cd "plugin/$p" && cargo build --release --target wasm32-wasip1 ); done'
    fi


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

# The full gate.
ci: fmt-check lint build build-plugins test coverage smoke nix-build
    @echo "ci: all green"

# The gate covers the testable core only (crate `superzej-core`). EXCLUDED: the
# exec / exit / subprocess seams that can't be unit-covered without real external
# tools (git/gh/podman/ssh) — exercised by smoke + e2e instead. See
# docs/coverage.md. Everything NOT matched here (config, keymap, db, theme,
# diff_highlight, models) is gated at 95% lines. The transitional cli, the native
# host, and the svc layer carry their own tests but are not part of this gate
# (their I/O-heavy surface is the same reason the seams above are excluded).
cov_ignore := 'superzej-core/src/(repo|worktree|sandbox|remote|github|picker|util|msg|out|log)\.rs'

# Coverage gate: 95% lines on the testable core. Writes lcov to target/coverage.
coverage:
    mkdir -p target/coverage
    cargo llvm-cov -p superzej-core --lib --fail-under-lines 95 \
      --ignore-filename-regex '{{cov_ignore}}' \
      --lcov --output-path target/coverage/lcov.info
    @echo "coverage: core ≥95% lines"

# Coverage as a browsable HTML report (target/llvm-cov/html).
coverage-html:
    cargo llvm-cov -p superzej-core --lib --html \
      --ignore-filename-regex '{{cov_ignore}}'

# Visual-regression: cell-grid snapshots of UI states via a sandboxed zellij.
# Compares against goldens in test/visual/ (≥95% cell similarity to pass).
visual: release
    python3 test/visual.py

# Regenerate the visual goldens (review the diff before committing).
visual-update: release
    python3 test/visual.py --update

# --- quality --------------------------------------------------------------

# Comprehensive linting: rust (clippy), bash (shellcheck), yaml (yamllint), toml (taplo).
lint: check-theme
    cargo clippy --workspace --all-targets -- -D warnings
    shellcheck -x install.sh test/smoke.sh test/gen-fixture.sh test/perf.sh test/one-session.sh test/slug-unique.sh
    yamllint .
    taplo lint

# Copy the canonical theme (crates/superzej-core/src/theme.rs) into each plugin
# crate. The plugins can't share a crate (the Nix builds sandbox each plugin
# subdir), so they carry committed copies; this keeps them in sync.
sync-theme:
    for p in sidebar tabbar panel statusbar; do \
      cp {{theme_src}} plugin/$p/src/theme.rs; done

# Fail if any plugin's theme.rs has drifted from the canonical core theme.rs.
check-theme:
    @for p in sidebar tabbar panel statusbar; do \
      diff -q {{theme_src}} plugin/$p/src/theme.rs \
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
    python3 test/palette-smoke.py {{bin}}

# Same, but against the built Nix package (verifies the wrapper + injected deps).
smoke-pkg:
    ./test/smoke.sh "$(nix build .#default --no-link --print-out-paths)/bin/superzej"

# End-to-end checks. one-session: superzej is ONE session and every repo is a
# tab, so opening/selecting never creates or switches a session (needs zellij).
# slug-unique: same-basename repos get distinct tab namespaces (hermetic).
e2e: release
    ./test/one-session.sh
    ./test/slug-unique.sh

# Real-zellij-on-a-pty UI tests. Self-contained (own sandbox HOME pointed at
# the freshly built binary + plugins) — never touches a live session. Needs a
# real `zellij` on PATH. resource-monitor: top-bar stat select -> embedded
# monitor. (nav-ux reads the *installed* plugins, so run it after `just install`.)
e2e-ui: release build-plugins
    python3 ./test/resource-monitor.py

# Visual + structural regression for the bottom file-manager drawer (needs
# zellij + yazi; SKIPs if absent). Sandboxed — never touches a real session.
drawer-e2e: release build-plugins
    python3 test/files-drawer.py
    SZ_TEST_DRAWER_WIDTH=center python3 test/files-drawer.py

# Combined unit + integration coverage with a 95% gate on the drawer code
# (yazi.rs, commands/files.rs, and the new zellij/config helpers). Captures the
# in-session paths (run/spawn/close/restore) the pty harness exercises.
coverage-drawer:
    ./test/coverage-drawer.sh

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
      SUPERZEJ_LAYOUT_DIR="$PWD/layouts" \
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
      "SUPERZEJ_LAYOUT_DIR=$PWD/layouts" \
      "SUPERZEJ_CONFIG=$PWD/config/zellij.kdl" \
      "SUPERZEJ_FRESH=1" \
      "$PWD/target/debug/superzej"

# --- stress fixture -------------------------------------------------------

# Generate a heavy, FULLY ISOLATED stress instance (~/.superzej-{{name}}): 20
# repos with random histories + 3-20 worktrees each (random ahead/behind/dirty),
# plus layout-stress.kdl pre-opening {{tabs}} worktree tabs. Idempotent; never
# touches your daily-driver superzej. See test/gen-fixture.sh.
stress-gen name="stress" repos="20" tabs="100": build
    ./test/gen-fixture.sh {{name}} {{repos}} {{tabs}}

# Launch the dev-tui against the stress instance in a fresh ghostty window
# (generates it first if missing). Same isolation as `start-term`, but reads the
# instance's own config + the generated heavy layout so the sidebar/tabbar are
# stressed by 20 repos and many open worktree tabs at once.
stress name="stress": build build-plugins
    [ -d "$HOME/.superzej-{{name}}/state" ] || ./test/gen-fixture.sh {{name}}
    -find "$HOME/.superzej-{{name}}/cache" -type d -name '*.wasm' -prune -exec rm -rf {} + 2>/dev/null
    setsid -f ghostty -e env \
      -u ZELLIJ -u ZELLIJ_SESSION_NAME -u ZELLIJ_PANE_ID \
      "PATH=$PWD/target/debug:$PATH" \
      "SUPERZEJ_ZELLIJ_BIN=$(nix build --no-link --print-out-paths .#zellij)/bin/zellij" \
      "SUPERZEJ_DIR=$HOME/.superzej-{{name}}" \
      "XDG_STATE_HOME=$HOME/.superzej-{{name}}/state" \
      "XDG_CONFIG_HOME=$HOME/.superzej-{{name}}/config" \
      "SUPERZEJ_LAYOUT=$HOME/.superzej-{{name}}/layout-stress.kdl" \
      "SUPERZEJ_LAYOUT_DIR=$PWD/layouts" \
      "SUPERZEJ_CONFIG=$PWD/config/zellij.kdl" \
      "SUPERZEJ_FRESH=1" \
      "$PWD/target/debug/superzej" new-workspace "$HOME/.superzej-{{name}}/fixtures/repos/east/washu"

# Perf regression check against the stress instance (asserts the `workspaces`
# sidebar feed stays fast). Run `just stress-gen` first.
perf name="stress": release
    ./test/perf.sh {{name}}

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
    cargo watch -w crates -w plugin -w layouts -w config -s "just start-term {{name}}"

# Remove build artifacts.
clean:
    cargo clean
    rm -f result result-*
