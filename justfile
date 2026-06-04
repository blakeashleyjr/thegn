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

# Build the Nix package; symlinks ./result.
nix-build:
    nix build .#default

# Print the store path without creating ./result.
path:
    @nix build .#default --no-link --print-out-paths

# Evaluate all flake outputs.
flake-check:
    nix flake check

# The full pre-commit gate.
ci: fmt-check lint build test smoke nix-build
    @echo "ci: all green"

# --- quality --------------------------------------------------------------

# Comprehensive linting: rust (clippy), bash (shellcheck), yaml (yamllint), toml (taplo).
lint:
    cargo clippy --all-targets -- -D warnings
    shellcheck -x install.sh test/smoke.sh
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

# Hermetic end-to-end test against the debug binary (no zellij side effects).
smoke: build
    ./test/smoke.sh {{bin}}

# Same, but against the built Nix package (verifies the wrapper + injected deps).
smoke-pkg:
    ./test/smoke.sh "$(nix build .#default --no-link --print-out-paths)/bin/superzej"

# --- run / install --------------------------------------------------------

# Run a subcommand against the debug build, e.g. `just run list --json`.
run *args: build
    {{bin}} {{args}}

# Build and run superzej locally — launches a real zellij session from the dev
# tree. Run it from a NON-zellij terminal (zellij can't nest); the dev binary is
# put on PATH so the layout's `superzej` calls resolve to this build.
start session="superzej-dev": build
    PATH="$PWD/target/debug:$PATH" \
      SUPERZEJ_LAYOUT="$PWD/layouts/superzej.kdl" \
      {{bin}} attach {{session}}

# Alias for `start`.
attach session="superzej-dev": (start session)

# Enter the dev shell.
dev:
    nix develop

# Remove build artifacts.
clean:
    cargo clean
    rm -f result result-*
