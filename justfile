# superzej — dev & build tasks. Run `just` to list, `just <recipe>` to run.
# Recipes assume the dev shell (`nix develop`) or the deps on PATH.

# The native compositor host (crate `superzej-host`); the shipped `superzej`.
bin := "target/debug/szhost"

# Show available recipes (default).
default:
    @just --list

# --- build / package ------------------------------------------------------

# Debug build (the whole cargo workspace: core, svc, host).
build:
    cargo build --workspace

# Release build (the whole cargo workspace).
release:
    cargo build --workspace --release

# Run the native host compositor. Builds it first. Run from a real terminal —
# it acquires raw mode and owns the screen.
host *args: build
    {{bin}} {{args}}

# Startup benchmarks (hyperfine; needs the dev shell). Not part of `just ci` —
# timings are machine-dependent. Three numbers: process/clap baseline; cold
# launch → first diff-flushed frame (fresh state: pays schema creation + first
# seed, i.e. the once-per-machine path); warm launch → first frame (existing
# state: the daily path). termwiz needs a PTY, so wrap in `script`, which adds
# a small constant overhead — fine for A/B deltas. Isolated XDG_STATE_HOME so
# the bench never touches the daily DB.
bench: release
    hyperfine --warmup 3 'target/release/szhost --version'
    hyperfine --warmup 3 --prepare 'rm -rf /tmp/sz-bench-state' \
      "script -qec 'env XDG_STATE_HOME=/tmp/sz-bench-state SUPERZEJ_BENCH_FIRST_FRAME_EXIT=1 target/release/szhost' /dev/null"
    hyperfine --warmup 3 \
      "script -qec 'env XDG_STATE_HOME=/tmp/sz-bench-state SUPERZEJ_BENCH_FIRST_FRAME_EXIT=1 target/release/szhost' /dev/null"

# Build the Nix package; symlinks ./result.
nix-build:
    nix build .#default

# Print the store path without creating ./result.
path:
    @nix build .#default --no-link --print-out-paths

# Evaluate all flake outputs.
flake-check:
    nix flake check

# The full gate.
ci: fmt-check lint build test coverage smoke nix-build
    @echo "ci: all green"

# (e2e/stress/perf harnesses drove the old zellij CLI's worktree-creation
# commands headlessly; worktree/workspace/pin creation is now an interactive
# compositor action, exercised by the host's unit tests.)

# The gate covers the testable core only (crate `superzej-core`). EXCLUDED: the
# exec / exit / subprocess seams that can't be unit-covered without real external
# tools (git/gh/podman/ssh) — exercised by smoke instead. See docs/coverage.md.
# Everything NOT matched here (config, db, theme, diff_highlight, models) is gated
# at 95% lines. The native host and the svc layer carry their own tests but are
# not part of this gate (their I/O-heavy surface is the same reason the seams
# above are excluded).
cov_ignore := 'superzej-core/src/(repo|worktree|sandbox|remote|github|picker|util|msg|out|log|plugin_api|forge/mod)\.rs'

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

# --- quality --------------------------------------------------------------

# Comprehensive linting: rust (clippy), bash (shellcheck), yaml (yamllint), toml (taplo).
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    shellcheck -x install.sh test/smoke.sh test/install-plan.sh test/dev-tui-plan.sh
    yamllint .
    taplo lint

# Format everything via treefmt (rust, nix, bash, toml, yaml, markdown).
fmt:
    nix fmt

# Check formatting without writing (CI-friendly).
fmt-check:
    nix fmt -- --ci

# Unit tests.
test:
    cargo test

# Hermetic end-to-end test against the debug binary.
smoke: build
    ./test/install-plan.sh
    ./test/dev-tui-plan.sh
    ./test/smoke.sh {{bin}}

# Same, but against the built Nix package (verifies the wrapper + injected deps).
smoke-pkg:
    ./test/smoke.sh "$(nix build .#default --no-link --print-out-paths)/bin/superzej"

# --- run / install --------------------------------------------------------

# Run a subcommand against the debug build, e.g. `just run list`.
run *args: build
    {{bin}} {{args}}

# Build and run the native host locally in an isolated state root.
start name="dev": build
    mkdir -p "$HOME/.superzej-{{name}}/state"
    XDG_STATE_HOME="$HOME/.superzej-{{name}}/state" \
      {{bin}}

# Alias for `start`.
attach: start

# Build and open the native host in a fresh ghostty window, with only the
# instance's isolated XDG state injected.
start-term name="dev": build
    mkdir -p "$HOME/.superzej-{{name}}/state"
    setsid -f ghostty -e env \
      "XDG_STATE_HOME=$HOME/.superzej-{{name}}/state" \
      "$PWD/{{bin}}"

# Install/update the native superzej host onto your PATH (standalone, non-Nix):
# builds release artifacts and symlinks `superzej`/`sj`/`szhost` to
# target/release/szhost. Pass a bindir to override the default (~/.local/bin),
# e.g. `just install ~/bin`.
install *bindir:
    ./install.sh {{bindir}}

# Enter the dev shell (default), or `just dev tui` for the auto-refreshing
# sandboxed TUI (see `dev-tui`).
dev what="shell":
    {{ if what == "tui" { "just dev-tui" } else { "nix develop" } }}

# Auto-refreshing native host TUI (also reachable as `just dev tui`). Watches
# Rust crates and, on every save, rebuilds/relaunches a fresh ghostty running the
# repo-local host. Runs once immediately; Ctrl-C stops the watcher.
# The watch set is scoped to source dirs, so build outputs don't retrigger it.
dev-tui name="dev":
    cargo watch -w crates -s "just start-term {{name}}"

# Remove build artifacts.
clean:
    cargo clean
    rm -f result result-*
