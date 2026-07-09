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

# Fast inner-loop check: typecheck + clippy on lib/bin code only (no test/bench
# targets, no tests, no coverage). Pass a crate to scope it further, e.g.
# `just quick superzej-host`. Use this while iterating; run the heavy gates
# (`just test` / `just coverage` / `just ci`) only when preparing to push/PR.
quick pkg="": _apps
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "{{pkg}}" ]; then scope="-p {{pkg}}"; else scope="--workspace"; fi
    cargo clippy $scope -- -D warnings

# Cross-platform regression gate: typecheck the C-dep-free leaf crates' per-OS
# code for macOS + Windows on this (Linux) box. `superzej-metrics` covers the
# sysinfo/battery substrate; `superzej-media` covers the per-OS player backends
# (Linux MPRIS/mpv, Windows SMTC, macOS AppleScript). `cargo check --target`
# needs no cross C toolchain (check never links). Targets are provided by the
# flake's rust toolchain. Catches the #1 cross-platform breakage — won't-compile
# — without macOS/Windows runners.
check-cross:
    cargo check -p superzej-metrics --target aarch64-apple-darwin
    cargo check -p superzej-metrics --target x86_64-pc-windows-gnu
    cargo check -p superzej-media --target aarch64-apple-darwin
    cargo check -p superzej-media --target x86_64-pc-windows-gnu

# Debug build of the host with the in-process sampling profiler compiled in
# (the `profiling` feature → SIGUSR2 flamegraph capture). Same artifact path as
# `build` (target/debug/szhost), so `start-term` picks it up transparently.
build-profiling: _apps
    cargo build --features profiling -p superzej-host

# Release build (the whole cargo workspace).
release: _apps
    cargo build --workspace --release

# Build a static x86_64-linux-musl `szhost` — the resident bridge binary pushed
# into Firecracker provider envs (Sprites). Self-contained (musl + bundled
# sqlite + rustls, no openssl) so it runs in a bare microVM. Needs the musl
# target (`rustup target add x86_64-unknown-linux-musl`) + a musl cross cc; in
# nix use `nix build .#szhost-musl` instead. Output:
# target/x86_64-unknown-linux-musl/release/szhost — point SUPERZEJ_BRIDGE_BINARY
# at it (or drop it next to the host exe as `szhost-musl`).
build-musl: _apps
    cargo build --release -p superzej-host --bin szhost --target x86_64-unknown-linux-musl

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

# --- spec-driven development (OpenSpec) -----------------------------------
# superzej manages its OWN development with OpenSpec (see openspec/, CLAUDE.md).
# The `openspec` binary is the hermetic, pinned build from nix/openspec.nix,
# provided on PATH by `nix develop`. tasks.md stays the roadmap index.

# Passthrough to the pinned openspec CLI (telemetry off). e.g. `just openspec list`.
openspec *args:
    OPENSPEC_TELEMETRY=0 DO_NOT_TRACK=1 openspec {{args}}

# (Re)generate the Claude Code /opsx commands + skills under .claude/ (gitignored,
# so each clone/worktree regenerates them). Run once after a fresh checkout.
openspec-setup:
    OPENSPEC_TELEMETRY=0 DO_NOT_TRACK=1 openspec init --tools claude --profile core --force

# Validate every spec and change strictly. Part of `ci`.
openspec-validate:
    OPENSPEC_TELEMETRY=0 DO_NOT_TRACK=1 openspec validate --all --strict

# The full gate.
ci: fmt-check lint deps-audit build check-cross test doc-check openspec-validate coverage smoke sandbox-e2e-dns sandbox-e2e-db e2e nix-build
    @echo "ci: all green"

# --- local CI (act) -------------------------------------------------------
# Run the GitHub Actions workflow (.github/workflows/ci.yml) locally in a
# container with `act`, to reproduce/debug the SERVER-side gate. This is HEAVY:
# every job installs nix in-container and cold-builds. For routine pre-push
# checks prefer `just ci` (or a single stage: `just lint` / `just test` /
# `just smoke`) — the CI jobs literally run `nix develop --command just <stage>`,
# so those give the same result without a container. See docs/local-ci.md.
#
# Needs: a running Docker (or podman) daemon + a `.secrets` file with
# NIX_GITHUB_TOKEN (copy .secrets.example). Config lives in .actrc.

_act-check:
    @command -v act >/dev/null 2>&1 || { echo "act not found — run inside 'nix develop' (or 'direnv allow'); it's in the dev-shell packages"; exit 1; }
    @test -f .secrets || { echo "no .secrets file — copy .secrets.example to .secrets and set NIX_GITHUB_TOKEN (see docs/local-ci.md)"; exit 1; }

# List the jobs act would run for the push event.
act-list:
    act -l

# Run the whole CI workflow locally (the `push` event the server gate runs on).
# Pass extra act flags after `--`, e.g. `just act -- --verbose`.
act *ARGS: _act-check
    act push {{ARGS}}

# Run a single CI job, e.g. `just act-job name=lint` or `just act-job name=test`.
act-job name: _act-check
    act push -j {{name}}

# Remove act's reused job containers (.actrc keeps them warm between runs);
# use this to reset a wedged/half-installed container.
act-clean:
    -docker ps -aq --filter 'name=act-' | xargs -r docker rm -f
    @echo "act containers removed"

# Dependency gates: security advisories, license policy, duplicate majors
# (cargo-deny; policy in deny.toml) and unused dependencies (cargo-machete).
# `cargo deny check advisories` fetches the RustSec DB, so this needs network
# on first run.
deps-audit:
    @for t in cargo-deny cargo-machete; do command -v "$t" >/dev/null 2>&1 || { echo "deps-audit: '$t' not found — run inside 'nix develop' (or 'direnv allow')"; exit 1; }; done
    cargo deny check
    cargo machete

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
    # muse takes spec FILES (a bare directory is "Is a directory" — os error 21).
    PATH="$(pwd)/target/debug:$PATH" muse run test/muse/specs/*.yaml \
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
    PATH="$(pwd)/target/debug:$PATH" muse run test/muse/specs/*.yaml \
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
cov_ignore := 'superzej-core/src/(repo|worktree|sandbox|sandbox_mounts|sandbox_prefetch|remote|github|picker|util|msg|out|log|devenv|direnv|plugin_api|profile|forge/mod)\.rs'

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
    @for t in shellcheck yamllint taplo; do command -v "$t" >/dev/null 2>&1 || { echo "lint: '$t' not found — run inside 'nix develop' (or 'direnv allow'); 'just doctor' for details"; exit 1; }; done
    cargo clippy --workspace --all-targets -- -D warnings
    shellcheck -x install.sh test/smoke.sh test/pty-smoke.sh test/install-plan.sh test/dev-tui-plan.sh test/sandbox-network.sh test/file-size-ratchet.sh test/git-hooks/post-checkout.sh test/git-hooks/heal-worktree.sh
    yamllint .
    taplo lint
    # Guardrail: all git must route through util::git_cmd / GitLoc so GIT_ENV_VARS
    # is scrubbed (the core.worktree-pollution class). Only the builder in util.rs
    # may call `git` directly; raw `Command::new("git")` anywhere else is rejected.
    # Comment lines are ignored (doc-comments legitimately name the pattern they forbid).
    ! grep -rIn 'Command::new("git")' crates --include='*.rs' | grep -v 'superzej-core/src/util.rs' | grep -vE ':[0-9]+:[[:space:]]*//' || (echo 'ERROR: raw Command::new("git") outside util::git_cmd — route through git_cmd/GitLoc to scrub GIT_ENV_VARS' && exit 1)
    # Guardrail: god-file ratchet — legacy oversized files may only shrink, new
    # files are hard-capped at 3000 lines. See test/file-size-ratchet.sh.
    bash test/file-size-ratchet.sh

# Repair a wedged checkout: strip a stray `core.worktree` that an external
# worktree tool (herdr) or a GIT_*-exporting child leaked into the shared
# `.git/config`. Symptom: `git add`/`commit`/`status` mis-target another tree,
# or (once the leaked path is deleted) git aborts with "Invalid path" / "must be
# run in a work tree". Pure-text repair — needs no working git, so it fixes the
# case a pre-commit hook can't (git dies before hooks run). Same key szhost heals
# in-process at startup + on worktree switch; this covers manual/CI git. No-op
# when clean.
heal-git:
    sh test/git-hooks/heal-worktree.sh -v || true
    @top=$(git rev-parse --show-toplevel 2>/dev/null) && echo "heal-git: ok — worktree $top" || echo "heal-git: git still wedged — inspect .git/config by hand"

# Diagnose the dev environment: report any missing toolchain bit with a one-line
# fix. Exits non-zero if anything is missing — handy for agents/CI to confirm the
# gates won't silently skip. (superzej panes get the devShell automatically via
# `[sandbox] inject_devshell`; this is for working ON superzej directly.)
doctor:
    #!/usr/bin/env bash
    set -uo pipefail
    echo "superzej dev-env doctor"
    miss=0
    check() { if command -v "$1" >/dev/null 2>&1; then echo "  ok    $1"; else echo "  MISS  $1 — $2"; miss=1; fi; }
    check nix            "install Nix (or you're on a non-Nix host)"
    check cargo          "rust toolchain — enter 'nix develop'"
    check just           "task runner — enter 'nix develop'"
    check shellcheck     "lint dep — enter 'nix develop' (or 'direnv allow')"
    check yamllint       "lint dep — enter 'nix develop'"
    check taplo          "lint/fmt dep — enter 'nix develop'"
    check treefmt        "formatter ('nix fmt') — enter 'nix develop'"
    check cargo-llvm-cov "coverage — enter 'nix develop'"
    if [ -z "${IN_NIX_SHELL:-}" ]; then
      echo "  note: not in a 'nix develop' shell (IN_NIX_SHELL unset)."
      echo "        Run 'nix develop', or 'direnv allow' (a .envrc is provided)."
    fi
    if [ "$miss" -eq 0 ]; then echo "all dev tools present ✔"; else echo "missing tools above — apply the fixes, then re-run 'just doctor'"; fi
    exit "$miss"

# Rustdoc must stay warning-clean; public API docs are part of the release gate.
doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace

# Format everything via treefmt (rust, nix, bash, toml, yaml, markdown).
fmt:
    nix fmt

# Check formatting without writing (CI-friendly).
fmt-check:
    nix fmt -- --ci

# Unit tests. cargo-nextest runs the suite with better parallelism than
# `cargo test`; it doesn't run doctests, so a `--doc` pass follows (a few crates
# carry `///` doctests). This recipe is the single source of truth shared by the
# CI `test` job and the pre-push hook.
test: _apps
    cargo nextest run --workspace
    cargo test --doc --workspace

# Formal verification (bounded model checking, CBMC via Kani) of the pure
# color-quantization math in `superzej-core::termcaps` (the `#[cfg(kani)]`
# proofs). Opt-in and machine-local: needs a one-time `cargo install --locked
# kani-verifier && cargo kani setup`. Deliberately NOT part of `just ci` — Kani's
# bundled CBMC toolchain is non-hermetic and the solve is slow.
#
# KNOWN BLOCKER (spike finding, 2026-07-07, kani 0.67.0): this does not currently
# compile. Kani must build all of `superzej-core`, and its transitive dep
# `libsqlite3-sys` (via `rusqlite`) uses the unstable `cfg_select!` in its build
# script, which Kani's pinned `nightly-2025-11-21` rejects. The 5 harnesses WERE
# verified (all SUCCESSFUL, ~1.2s) by extracting the color fns verbatim into a
# standalone dep-free crate. To run in-tree, either Kani's toolchain must advance
# past that dep, or the pure-math module must be split into a leaf crate with no
# heavy deps. Until then, treat the `#[cfg(kani)]` proofs as documentation.
verify-kani:
    cargo kani -p superzej-core

# Live integration tests against the REAL Sprites API (creates + destroys throwaway
# sprites — real cloud spend). Sources SPRITES_TOKEN from .envrc.local. Validates
# the provider exec/fs/checkpoint primitives + the env-provisioning clone path that
# back the transparent sandbox/remote feature. `#[ignore]` so normal `just test`
# skips them.
test-sprite:
    [ -f .envrc.local ] && set -a && . ./.envrc.local && set +a; \
      [ -n "${SPRITES_TOKEN:-}" ] || { echo "SPRITES_TOKEN not set (put it in .envrc.local)" >&2; exit 1; }; \
      cargo test -p superzej-svc --test sprites_live -- --ignored --nocapture

# Live sprite-recycle verification (hosts-as-resources S1/S2): checkpoint
# capture, stale restore-in-place, claimed-delete round trip, bad-checkpoint
# fallback. Real cloud spend; serial (the tests hold the crate env lock).
sprites-live-recycle:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -f .envrc.local ] && . ./.envrc.local
    [ -n "${SPRITES_TOKEN:-}" ] || { echo "SPRITES_TOKEN not set (put it in .envrc.local)" >&2; exit 1; }
    cargo test -p superzej-host --bin szhost live_recycle -- --ignored --nocapture --test-threads=1

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

# Headless terminal-capability matrix: run `szhost doctor` under a set of
# degraded environments (each a clean `env -i`, so the outer terminal's
# COLORTERM / TERM_PROGRAM can't leak in and mask a degradation) and assert the
# resolved color depth + glyph level match what each terminal should get. Proves
# the graceful-degradation layer (`superzej_core::termcaps`) end to end without a
# tty, complementing the pure unit tests. For the real rendered proof, launch
# `just start-term` under a degraded TERM (e.g. `TERM=xterm LANG=C`).
term-check: build
    #!/usr/bin/env bash
    set -euo pipefail
    bin="$PWD/{{bin}}"
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
    base=(PATH="$PATH" HOME="$tmp" XDG_STATE_HOME="$tmp/state" XDG_CONFIG_HOME="$tmp/cfg")
    fail=0
    # check <name> <want-color> <want-glyph> <env...>
    check() {
      local name="$1" ec="$2" eg="$3"; shift 3
      local out; out="$(env -i "${base[@]}" "$@" "$bin" doctor 2>&1)"
      local caps color glyph
      caps="$(printf '%s\n' "$out" | sed -n '/Resolved capabilities/,/Summary/p')"
      color="$(printf '%s\n' "$caps" | awk '/^  color /{print $2; exit}')"
      glyph="$(printf '%s\n' "$caps" | awk '/^  glyphs /{print $2; exit}')"
      if [ "$color" = "$ec" ] && [ "$glyph" = "$eg" ]; then
        printf '  PASS  %-11s color=%-10s glyphs=%s\n' "$name" "$color" "$glyph"
      else
        printf '  FAIL  %-11s color=%s (want %s)  glyphs=%s (want %s)\n' \
          "$name" "$color" "$ec" "$glyph" "$eg"; fail=1
      fi
    }
    echo "terminal-capability matrix (szhost doctor, clean env):"
    check kitty      truecolor  full  TERM=xterm-kitty COLORTERM=truecolor LANG=en_US.UTF-8
    check bare       16-color   ascii TERM=xterm LANG=C
    check no-color   monochrome full  TERM=xterm-kitty COLORTERM=truecolor NO_COLOR=1 LANG=en_US.UTF-8
    check 256color   256-color  basic TERM=xterm-256color LANG=en_US.UTF-8
    check glyph=asci truecolor  ascii TERM=xterm-kitty COLORTERM=truecolor LANG=en_US.UTF-8 SUPERZEJ_THEME_GLYPHS=ascii
    check color=16   16-color   full  TERM=xterm-kitty COLORTERM=truecolor LANG=en_US.UTF-8 SUPERZEJ_THEME_COLOR=16
    if [ "$fail" = 0 ]; then echo "term-check: all green"; else echo "term-check: MISMATCH"; exit 1; fi

# Point THIS repo (all its worktrees) at a sandbox backend / managed-sandbox
# provider, without touching any other repo — the engine behind the `backend=`
# param on `start-term`/`start-term-release`. Writes a per-repo `.superzej.toml`
# overlay at the MAIN worktree (resolved via `git --git-common-dir`, mirroring
# `superzej_core::repo::main_worktree`), which every superzej worktree reads.
#   - `sprites` (a managed-sandbox PROVIDER) → overlay `env = "sprites"`, and the
#     global `[env.sprites]` block is auto-scaffolded into
#     $XDG_CONFIG_HOME/superzej/config.toml if missing. Needs SPRITES_TOKEN set
#     (fail-fast); the `sprite` CLI is only advisory now — the default exec=auto
#     attaches the pane over the native WSS exec API with no vendor CLI.
#   - a real sandbox backend (podman|docker|bwrap|systemd|none) → overlay
#     `[sandbox] backend = "<x>"`.
# Empty `backend` is a no-op. Refuses to clobber a hand-authored overlay that
# already carries a `[keybinds]`/`[sandbox]` table. To revert: delete the overlay
# (`rm "$(git rev-parse --show-toplevel)/.superzej.toml"` from the main checkout).
_apply-backend backend="":
    backend="{{backend}}"; \
    if [ -n "$backend" ]; then \
      root="$(git rev-parse --path-format=absolute --git-common-dir)"; \
      case "$root" in */.git) root="${root%/.git}";; esac; \
      overlay="$root/.superzej.toml"; \
      cfg="${XDG_CONFIG_HOME:-$HOME/.config}/superzej/config.toml"; \
      if [ -f "$overlay" ] && grep -qE '^\[(keybinds|sandbox)' "$overlay"; then \
        echo "$overlay already has a [keybinds]/[sandbox] table — edit it by hand to set the backend" >&2; exit 1; \
      fi; \
      case "$backend" in \
        sprites) \
          [ -n "${SPRITES_TOKEN:-}" ] || { echo "SPRITES_TOKEN not set — export it before using backend=sprites" >&2; exit 1; }; \
          command -v sprite >/dev/null 2>&1 || echo "note: sprite CLI not on PATH — fine for native exec (the default exec=auto attaches over the WSS API with just SPRITES_TOKEN); the CLI is only the fallback transport" >&2; \
          if ! grep -q '^\[env\.sprites\]' "$cfg" 2>/dev/null; then \
            mkdir -p "$(dirname "$cfg")"; \
            printf '\n[env.sprites]\nplacement = "provider"\ndata = "in_env"\n[env.sprites.provider]\nprovider = "sprites"\n# Per-worktree sprite name. Tokens: {repo}=repo name, {worktree}=dir basename,\n# {hash}=stable path digest (collision-defuser), {slug}=full-path. Default is\n# conflict-free across repos; "" is equivalent. A no-token literal = one shared sprite.\nid = "{repo}-{worktree}-{hash}"\napi_key_env = "SPRITES_TOKEN"\nexec = "auto"\nauto_provision = true\nauto_checkpoint = true\n# CLI bridge — only used if you set exec = "cli"; lifecycle is API (auto_provision/auto_checkpoint).\nexec_command = ["sprite", "exec", "-s", "{id}", "--"]\ninteractive_command = ["sprite", "exec", "-s", "{id}", "--tty", "--"]\n' >> "$cfg"; \
            echo "scaffolded [env.sprites] into $cfg"; \
          fi; \
          printf 'env = "sprites"\n' > "$overlay"; \
          echo "set $overlay -> env = \"sprites\" (applies to all worktrees under $root)"; \
          ;; \
        podman|docker|bwrap|systemd|none) \
          printf '[sandbox]\nbackend = "%s"\n' "$backend" > "$overlay"; \
          echo "set $overlay -> [sandbox] backend = \"$backend\" (applies to all worktrees under $root)"; \
          ;; \
        *) echo "unknown backend '$backend' — expected: sprites | podman | docker | bwrap | systemd | none" >&2; exit 1;; \
      esac; \
    fi

# Dogfood the local merge queue (the fold-actor): same isolated state root as
# `start`, but with `[merge_queue]` switched on via --set overrides (no daily
# config edit needed). Folds eligible worktree branches into the target branch
# with a compile gate; Super+K → "Integrate" / "Merge queue", or it auto-drains
# ~5s after an agent finishes. Override the gate with `gate='just test'` etc.
start-mq name="dev" gate="cargo build --workspace": build
    state="$HOME/.superzej-{{name}}/state"; run="$HOME/.superzej-{{name}}/run"; pidfile="$run/szhost.pid"; mkdir -p "$state" "$run"; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      echo $$ > "$pidfile"; exec env \
      "SUPERZEJ_ALACRITTY_CONFIG=$PWD/config/alacritty.toml" \
      "XDG_STATE_HOME=$state" \
      {{bin}} --set merge_queue.enabled=true \
              --set 'merge_queue.gate_command={{gate}}' \
              --set merge_queue.regenerate_command="cargo update --workspace"

# Build and open the native host in a fresh ghostty window with the FULL
# dev/debug/profiling toolchain wired up. ghostty runs a hermetic, perf-tuned
# profile (config/ghostty.config: --config-default-files=false keeps your
# personal ghostty config out): no decorations/scrollback/URL-detection, vsync
# off for minimum input-to-present latency, and a dedicated single-instance
# process so the pidfile + `pgrep -n szhost` + SIGUSR2 drill all hit it. Plus:
#   - binary built with the `profiling` feature → SIGUSR2 flamegraph capture
#     (kill -USR2 once to start sampling, again to dump);
#   - every instrumentation channel on: startup waterfall + frame + hydrate +
#     perf logs land in $XDG_STATE_HOME/superzej/logs/szhost.log, and the
#     runtime self-profiler rollup is enabled (SUPERZEJ_PERF=1);
#   - state stays isolated per instance (~/.superzej-<name>).
# NOTE: this is a DEBUG binary (~2.5x slower than release), so read the
# flamegraph/perf rollup for structure & relative cost — for absolute timings
# use the release-grade `just bench` / `just bench-idle` harnesses.
# Optional `backend=` flips THIS repo's worktrees onto a sandbox backend /
# managed provider (e.g. `just start-term dev backend=sprites`) — see
# `_apply-backend`. Empty (the default) leaves config untouched.
start-term name="dev" backend="": build-profiling (_apply-backend backend)
    state="$HOME/.superzej-{{name}}/state"; run="$HOME/.superzej-{{name}}/run"; pidfile="$run/szhost.pid"; mkdir -p "$state" "$run"; \
      if [ -f "$PWD/.envrc.local" ]; then set -a; . "$PWD/.envrc.local"; set +a; fi; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      echo "profiler: 'kill -USR2 \$(pgrep -n szhost)' to start sampling, again to dump → $state/superzej/profiles/"; \
      echo "logs: $state/superzej/logs/szhost.log (startup waterfall + frame/hydrate/perf)"; \
      setsid -f ghostty --config-default-files=false --config-file="$PWD/config/ghostty.config" -e sh -lc \
      'pidfile="$1"; shift; echo $$ > "$pidfile"; exec env "$@"' \
      sh "$pidfile" \
      "XDG_STATE_HOME=$state" \
      "SPRITES_TOKEN=${SPRITES_TOKEN:-}" \
      "SUPERZEJ_LOG=info,szhost::frame=debug,szhost::hydrate=debug,szhost::perf=debug" \
      "SUPERZEJ_PERF=1" \
      "$PWD/{{bin}}"

# Same dev/profiling/instrumentation rig as `start-term`, but a RELEASE binary —
# the daily-driver launcher. `start-term` stays debug for fast `cargo watch`
# rebuilds (`just dev-tui`); this gets the ~2.5x release speedup while keeping
# every log channel + the SIGUSR2 flamegraph profiler on, so live perf readings
# (frame render_ms, the szhost::perf rollup, idle CPU) reflect real shipped cost
# instead of the debug penalty. Use this to inhabit superzej all day.
# LOGGING IS MAXED OUT here for crash diagnosis: SUPERZEJ_LOG=debug globally with
# all superzej crates at trace → $logs/szhost.log, RUST_BACKTRACE=full, and the
# host's stderr (where panics print, normally swallowed when the ghostty window
# closes on a crash) is captured to $logs/stderr.log. After a crash, read
# stderr.log first (the panic + backtrace), then szhost.log for the lead-up.
# Optional `backend=` flips THIS repo's worktrees onto a sandbox backend /
# managed provider before launch — `just start-term-release backend=sprites`
# dogfoods the superzej repo onto sprites (auto-scaffolds the global
# `[env.sprites]`; needs the `sprite` CLI + SPRITES_TOKEN). Also accepts a real
# sandbox backend: podman|docker|bwrap|systemd|none. Affects ONLY this repo (a
# `.superzej.toml` overlay at the main worktree); empty leaves config untouched.
# See `_apply-backend` for the full mechanics.
start-term-release name="dev" backend="": release-profiling (_apply-backend backend)
    state="$HOME/.superzej-{{name}}/state"; run="$HOME/.superzej-{{name}}/run"; pidfile="$run/szhost.pid"; logs="$state/superzej/logs"; mkdir -p "$state" "$run" "$logs"; \
      if [ -f "$PWD/.envrc.local" ]; then set -a; . "$PWD/.envrc.local"; set +a; fi; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      echo "profiler: 'kill -USR2 \$(pgrep -n szhost)' to start sampling, again to dump → $state/superzej/profiles/"; \
      echo "logs: $logs/szhost.log (full trace: startup/frame/hydrate/perf + every crate) + $logs/stderr.log (panic message + full backtrace)"; \
      echo "sprites token: $([ -n "${SPRITES_TOKEN:-}" ] && echo "loaded (len ${#SPRITES_TOKEN})" || echo "NOT set — sprites envs will halt; put SPRITES_TOKEN in .envrc.local")"; \
      setsid -f ghostty --config-default-files=false --config-file="$PWD/config/ghostty.config" -e sh -lc \
      'pidfile="$1"; errlog="$2"; shift 2; echo $$ > "$pidfile"; exec env "$@" 2>"$errlog"' \
      sh "$pidfile" "$logs/stderr.log" \
      "XDG_STATE_HOME=$state" \
      "SPRITES_TOKEN=${SPRITES_TOKEN:-}" \
      "RUST_BACKTRACE=full" \
      "SUPERZEJ_LOG=debug,szhost=trace,superzej_core=trace,superzej_svc=trace,superzej_proxy=trace" \
      "SUPERZEJ_PERF=1" \
      "$PWD/target/release/szhost"

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

# --- sandbox base image (hosts-as-resources) ---------------------------------

# Build the base sandbox image for THIS machine's arch and load it into podman
# (`nix/sandbox-image.nix` → streamLayeredImage). The provisioner then delivers
# it registry-lessly to hosts (`superzej host provision <name>`).
image-build:
    nix build .#sandbox-image
    ./result | podman load

# Publish both arches + a manifest list to a registry, then print the list
# digest to pin as DEFAULT_BASE_DIGEST (superzej-core/src/image.rs). Needs
# native builders per arch (or remote builders); run on CI normally.
#   just image-publish registry=ghcr.io/you tag=v1
image-publish registry tag="v1":
    #!/usr/bin/env bash
    set -euo pipefail
    ref="{{registry}}/superzej-sandbox:{{tag}}"
    arch="$(uname -m)"; case "$arch" in x86_64) oci=amd64;; aarch64) oci=arm64;; *) echo "unsupported arch $arch" >&2; exit 1;; esac
    nix build .#sandbox-image
    ./result | podman load
    podman tag superzej-sandbox:latest "$ref-$oci"
    podman push "$ref-$oci"
    echo "pushed $ref-$oci — repeat on the other arch, then:"
    echo "  podman manifest create $ref $ref-amd64 $ref-arm64 && podman manifest push $ref"
    echo "  (the printed manifest-list digest pins DEFAULT_BASE_DIGEST)"

# Build the Fly.io boot image (sshd entrypoint + baked nix/rust/just) and push it
# to a registry Fly can pull, so a Fly machine boots STRAIGHT into a reachable
# shell with the toolchain baked — no per-VM install. Then set the printed
# `template` on the env. Run on a machine with a writable /nix + podman.
#   just fly-image-publish registry=ghcr.io/you tag=v1
fly-image-publish registry tag="v1":
    #!/usr/bin/env bash
    set -euo pipefail
    ref="{{registry}}/superzej-fly-sandbox:{{tag}}"
    nix build .#fly-sandbox-image
    ./result | podman load
    podman tag superzej-fly-sandbox:latest "$ref"
    podman push "$ref"
    echo "pushed $ref — point the Fly env at it:"
    echo "  [env.fly.provider]"
    echo "  template = \"image:$ref\"   # boots from the baked image (fast path)"
