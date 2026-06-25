# superzej — dev & build tasks. Run `just` to list, `just <recipe>` to run.
# Recipes assume the dev shell (`nix develop`) or the deps on PATH.

# The native compositor host (crate `superzej-host`); the shipped `superzej`.
bin := "target/debug/szhost"

# Hermetic-environment preamble for the e2e recipes: redirect HOME, the XDG dirs,
# and git config into a throwaway temp dir (cleaned on exit) so the visual suite
# can neither read the developer's real config/gitconfig nor leak test state into
# the daily DB. Specs further isolate XDG_STATE_HOME per case via case_tmp_env.
_e2e_env := '''
set -euo pipefail
_tmp="$(mktemp -d)"; trap 'rm -rf "$_tmp"' EXIT
export HOME="$_tmp/home" XDG_CONFIG_HOME="$_tmp/config" XDG_STATE_HOME="$_tmp/state"
export GIT_CONFIG_GLOBAL="$_tmp/gitconfig" GIT_CONFIG_SYSTEM=/dev/null
mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"
printf '[user]\nname = e2e\nemail = e2e@example.invalid\n' > "$_tmp/gitconfig"
'''

# Show available recipes (default).
default:
    @just --list

# --- build / package ------------------------------------------------------

# Self-heal the embedded-app sources the host's path deps need (termite-chat,
# termite-agent under apps/). These live ONLY in the canonical checkout: apps/
# is gitignored + untracked, and the submodules need network/SSH (bwrap blocks
# it), so a fresh `git worktree` has no apps/ at all and the whole workspace
# fails to resolve — not just the host. Symlink to the shared checkout, derived
# from the common git dir (no hardcoded path); apps/ is gitignored so the link
# never shows in `git status`. No-op in the canonical checkout (apps/ is a real
# dir) or once already linked. This is what lets agents in sandboxed worktrees
# build without copying apps/ in by hand.
_apps:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -e apps ] && exit 0
    root="$(cd "$(dirname "$(git rev-parse --git-common-dir)")" && pwd)"
    if [ "$root" != "$PWD" ] && [ -d "$root/apps" ]; then
      ln -s "$root/apps" apps
      echo "linked apps/ -> $root/apps (embedded-app sources for the host build)"
    else
      echo "warning: no apps/ here and none at $root/apps — host chat/agent tabs will not build" >&2
    fi

# Debug build (the whole cargo workspace: core, svc, host).
build: _apps
    cargo build --workspace

# Release build (the whole cargo workspace).
release: _apps
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

# Guard run by every perf recipe: refuse to measure a debug or stale binary.
# The debug-vs-release CPU gap is ~2.5x (and cargo test/clippy don't rebuild
# target/debug/szhost), so a perf number from the wrong binary is worse than
# none. Prints the resolved binary + mtime + profile so reports self-describe.
_perf-guard:
    #!/usr/bin/env bash
    set -euo pipefail
    b="target/release/szhost"
    if [ ! -x "$b" ]; then
      echo "perf: $b not built — run 'just release' first" >&2; exit 1
    fi
    src_newest="$(find crates apps -name '*.rs' -newer "$b" 2>/dev/null | head -1 || true)"
    if [ -n "$src_newest" ]; then
      echo "perf: $b is STALE (newer source: $src_newest) — run 'just release'" >&2; exit 1
    fi
    echo "perf: binary=$b mtime=$(date -r "$b" '+%F %T') profile=release"

# Idle CPU benchmark: launch szhost in a PTY over a fixture of N worktrees, let
# it settle, sample /proc CPU over a window, and assert it stays under the
# 0%-idle ceiling. This is the steady-state cost `just bench` never sees.
bench-idle: release _perf-guard
    bash test/perf/cpu-sample.sh --scenario idle

# Record the current idle reading as this machine's baseline (machine-scoped).
bench-idle-record: release _perf-guard
    bash test/perf/cpu-sample.sh --scenario idle --record

# Steady-workload CPU benchmark (A/B only — feeds scripted keystrokes).
bench-steady: release _perf-guard
    bash test/perf/cpu-sample.sh --scenario steady-workload --window-ms 6000

# Criterion micro-benchmarks across the workspace (hot git path, core models).
# `cargo bench` uses the release-grade bench profile. For a debug-vs-release
# A/B, append `--profile dev`. Pass extra criterion args after `--`.
bench-micro *args:
    cargo bench --workspace {{args}}

# Just the git hot-path benches (is_dirty / ahead_behind / current_branch,
# gix vs CLI, scaled by worktree count) — the dominant idle cost.
bench-micro-svc *args:
    cargo bench -p superzej-svc --bench git_hot {{args}}

# Umbrella: startup (hyperfine) + idle CPU + micro-benches. Self-describing
# (each sub-recipe prints its binary/profile). Machine-dependent — not in CI.
perf: bench bench-idle bench-micro
    @echo "perf: startup + idle + micro complete"

# Build szhost with the in-process sampling profiler (release + profiling
# feature). SIGUSR2 toggles a flamegraph capture written to
# $XDG_STATE_HOME/superzej/profiles/. Profiles the live process (sidesteps
# ptrace_scope=1, which blocks external perf/gdb attach).
release-profiling:
    cargo build --release --features profiling -p superzej-host

# Launch szhost under the profiler and print how to drive it. Run from a real
# terminal. `kill -USR2 <pid>` once to start sampling, again to dump.
profile *args: release-profiling
    #!/usr/bin/env bash
    set -euo pipefail
    echo "profiler: send 'kill -USR2 \$(pgrep -n szhost)' to start, again to dump."
    echo "profiles land in \$XDG_STATE_HOME/superzej/profiles/ (or ~/.local/state/...)."
    SUPERZEJ_LOG=szhost::perf=info target/release/szhost {{args}}

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
ci: fmt-check lint build test doc-check coverage smoke sandbox-e2e-dns sandbox-e2e-db e2e nix-build
    @echo "ci: all green"

# Visual regression suite: run all muse specs against a live szhost binary.
# Baselines live in test/muse/snapshots/ and are committed to git.
# Workers default to 4; glitch-hunt specs run serial (--workers 1) to avoid
# UI-state races between concurrently running szhost processes.
# szhost is put on PATH so specs can use spawn: ["szhost"] portably.
#
# The suite is hermetic w.r.t. the developer's environment: `_e2e_env` isolates
# HOME, the XDG dirs, and git config into a throwaway temp dir (cleaned on exit),
# so warm/shared envs can neither change behavior nor leak test state. Each spec
# additionally isolates XDG_STATE_HOME per case via `case_tmp_env`.
e2e: build
    #!/usr/bin/env bash
    {{_e2e_env}}
    PATH="$(pwd)/target/debug:$PATH" muse run test/muse/specs/ \
        --reporter pretty --workers 4 --deadline-ms 12000

# Run only the glitch-hunt specs (18–28) — slower, more thorough.
e2e-glitch: build
    #!/usr/bin/env bash
    {{_e2e_env}}
    PATH="$(pwd)/target/debug:$PATH" muse run \
        test/muse/specs/1[89]-*.yaml test/muse/specs/2[0-9]-*.yaml \
        --reporter pretty --workers 2 --deadline-ms 12000

# Update snapshot baselines (run after intentional rendering changes).
e2e-update: build
    #!/usr/bin/env bash
    {{_e2e_env}}
    PATH="$(pwd)/target/debug:$PATH" muse run test/muse/specs/ \
        --update-snapshots --workers 4 --deadline-ms 12000

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

# The LLM-proxy crate is gated separately at 88% lines (its decision logic lives
# in the 95%-gated core::proxy; this covers the I/O shell — router, server, relay,
# upstream — via unit + integration (`tests/e2e.rs`) tests, hence `--tests`).
# EXCLUDED: `main.rs`/`lib.rs` — the bind+serve loop, signal handling, and binary
# entry can't be unit-covered (same rationale as core's seams; exercised live).
proxy_cov_ignore := 'superzej-proxy/src/(main|lib)\.rs'

# Coverage gate: core ≥95% lines + proxy ≥88% lines. Writes lcov to target/coverage.
coverage: _apps
    mkdir -p target/coverage
    # Discard any stale .profraw from earlier instrumented runs — merging them
    # produces a false-low (or false-high) line %, which can spuriously fail the
    # gate locally (CI's clean checkout never sees this).
    cargo llvm-cov clean --workspace
    cargo llvm-cov -p superzej-core --lib --fail-under-lines 95 \
      --ignore-filename-regex '{{cov_ignore}}' \
      --lcov --output-path target/coverage/lcov.info
    @echo "coverage: core ≥95% lines"
    cargo llvm-cov -p superzej-proxy --lib --tests --fail-under-lines 88 \
      --ignore-filename-regex '{{proxy_cov_ignore}}' \
      --lcov --output-path target/coverage/lcov-proxy.info
    @echo "coverage: proxy ≥88% lines"

# Coverage as a browsable HTML report (target/llvm-cov/html).
coverage-html:
    cargo llvm-cov -p superzej-core --lib --html \
      --ignore-filename-regex '{{cov_ignore}}'

# --- quality --------------------------------------------------------------

# Comprehensive linting: rust (clippy), bash (shellcheck), yaml (yamllint), toml (taplo).
lint: _apps
    cargo clippy --workspace --all-targets -- -D warnings
    shellcheck -x install.sh test/smoke.sh test/pty-smoke.sh test/install-plan.sh test/dev-tui-plan.sh test/sandbox-network.sh test/git-hooks/post-checkout.sh
    yamllint .
    taplo lint
    # Guardrail: all git must route through util::git_cmd / GitLoc so GIT_ENV_VARS
    # is scrubbed (the core.worktree-pollution class). Only the builder in util.rs
    # may call `git` directly; raw `Command::new("git")` anywhere else is rejected.
    ! grep -rIn 'Command::new("git")' crates --include='*.rs' | grep -v 'superzej-core/src/util.rs' || (echo 'ERROR: raw Command::new("git") outside util::git_cmd — route through git_cmd/GitLoc to scrub GIT_ENV_VARS' && exit 1)

# Rustdoc must stay warning-clean; public API docs are part of the release gate.
doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace

# Format everything via treefmt (rust, nix, bash, toml, yaml, markdown).
fmt:
    nix fmt

# Check formatting without writing (CI-friendly).
fmt-check:
    nix fmt -- --ci

# Unit tests.
test: _apps
    cargo test

# Hermetic end-to-end test against the debug binary.
smoke: build
    ./test/install-plan.sh
    ./test/dev-tui-plan.sh
    ./test/smoke.sh {{bin}}
    ./test/pty-smoke.sh {{bin}}

# Sandbox integration tests: require podman (or docker) to be available.
# Set PODMAN_E2E_FORCE=1 to assert it must pass (CI with podman).
# Without the env var the recipe just reports "podman not found, skipping."
sandbox-e2e: build
    @if command -v podman >/dev/null 2>&1; then \
      echo "sandbox-e2e: podman found, running integration tests"; \
      PODMAN_E2E_FORCE=1 cargo test -p superzej-core -- sandbox; \
    elif [ "$$PODMAN_E2E_FORCE" = "1" ]; then \
      echo "sandbox-e2e: PODMAN_E2E_FORCE=1 but podman not found"; exit 1; \
    else \
      echo "sandbox-e2e: podman not found, skipping (set PODMAN_E2E_FORCE=1 to fail on missing)"; \
    fi

# DNS filter E2E — Tier 1, no podman needed; always runs in CI.
sandbox-e2e-dns:
    cargo test -p superzej-core --test sandbox_dns_e2e

# DB audit trail — Tier 1, no podman; always runs in CI.
sandbox-e2e-db:
    cargo test -p superzej-core --lib -- db::tests::container_events

# Full podman-backed suite (Tier 2). Discovers podman and exits cleanly if absent.
sandbox-e2e-full: build
    @if command -v podman >/dev/null 2>&1; then \
      echo "sandbox-e2e-full: podman found, running Tier 2 tests"; \
      PODMAN_E2E_FORCE=1 cargo test -p superzej-core \
        --test sandbox_lifecycle \
        --test sandbox_credentials \
        --test sandbox_health \
        --test sandbox_network_policy \
        --test sandbox_audit \
        --test sandbox_profile; \
    elif [ "$$PODMAN_E2E_FORCE" = "1" ]; then \
      echo "sandbox-e2e-full: PODMAN_E2E_FORCE=1 but podman not found"; exit 1; \
    else \
      echo "sandbox-e2e-full: podman not found — skipping Tier 2 tests"; \
    fi

# Same, but against the built Nix package (verifies the wrapper + injected deps).
smoke-pkg:
    ./test/smoke.sh "$(nix build .#default --no-link --print-out-paths)/bin/superzej"

# --- run / install --------------------------------------------------------

# Run a subcommand against the debug build, e.g. `just run list`.
run *args: build
    {{bin}} {{args}}

# Build and run the native host locally in an isolated state root.
start name="dev": build
    state="$HOME/.superzej-{{name}}/state"; run="$HOME/.superzej-{{name}}/run"; pidfile="$run/szhost.pid"; mkdir -p "$state" "$run"; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      echo $$ > "$pidfile"; exec env \
      "SUPERZEJ_ALACRITTY_CONFIG=$PWD/config/alacritty.toml" \
      "XDG_STATE_HOME=$state" \
      {{bin}}

# Alias for `start`.
attach: start

# Build and open the native host in a fresh alacritty window (optimized
# profile: no decorations, no outer scrollback — the host owns both), with only
# the instance's isolated XDG state injected.
start-term name="dev": build
    state="$HOME/.superzej-{{name}}/state"; run="$HOME/.superzej-{{name}}/run"; pidfile="$run/szhost.pid"; mkdir -p "$state" "$run"; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      setsid -f alacritty --config-file "$PWD/config/alacritty.toml" -e sh -lc \
      'pidfile="$1"; shift; echo $$ > "$pidfile"; exec env "$@"' \
      sh "$pidfile" \
      "SUPERZEJ_ALACRITTY_CONFIG=$PWD/config/alacritty.toml" \
      "XDG_STATE_HOME=$state" \
      "$PWD/{{bin}}"

# Install/update the native superzej host onto your PATH (standalone, non-Nix):
# builds release artifacts, installs `sj` as the dedicated alacritty launcher,
# `sj-tui` for the current terminal window, and direct `superzej`/`szhost`
# native-host aliases. Pass a bindir to override the default (~/.local/bin),
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

# --- fonts ------------------------------------------------------------------

# Installed Nerd Font families (candidates for `just font`).
fonts:
    @fc-list : family | tr ',' '\n' | grep -i 'nerd font' | grep -iv 'mono\b.*propo\|propo' | sort -u

# Switch the bundled alacritty profile's font live (alacritty live-reloads,
# so the change is instant in a running session). e.g.
#   just font name="JetBrainsMono Nerd Font"
font name:
    sed -i 's/^normal = { family = ".*" }$/normal = { family = "{{name}}" }/' config/alacritty.toml
    @echo "font → {{name}} (alacritty live-reloads in place)"
