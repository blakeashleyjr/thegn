# thegn — dev & build tasks. Run `just` to list, `just <recipe>` to run.
# Recipes assume the dev shell (`nix develop`) or the deps on PATH.

# The native compositor host (crate `thegn-host`); the shipped `thegn`.
bin := "target/debug/thegn"

# Detach a long-running GUI process from this shell, portably. `setsid` is
# util-linux and absent on macOS; `nohup … &` is the POSIX equivalent that
# survives the shell exiting. Kept to ONE line: recipes splice it into a
# backslash-continued command list, where a multi-line function body would break
# the continuation. POSIX `sh` only (just's default shell) — no `disown`, which
# is a bash/zsh builtin and unnecessary in a non-interactive shell anyway.
_detach := '_detach() { if command -v setsid >/dev/null 2>&1; then setsid -f "$@"; else nohup "$@" >/dev/null 2>&1 & fi; };'

# Hermetic-environment preamble for the e2e recipes: redirect HOME, the XDG dirs,
# and git config into a throwaway temp dir (cleaned on exit) so the suite can
# neither read the developer's real config/gitconfig nor leak test state into
# the daily DB. Specs further isolate XDG_STATE_HOME per case via case_tmp_env.
# Failure artifacts (muse's test-results/) are written OUTSIDE the temp dir, to
# `e2e-results/` in the repo, so they survive the cleanup and CI can upload them.
_e2e_env := '''
set -euo pipefail
_tmp="$(mktemp -d)"
# The cleanup must never decide the verdict: a root-owned leftover under the
# temp HOME (podman overlay dirs have done this on CI) would otherwise turn a
# 60/60 green suite into a red gate with an `rm: Permission denied` as the
# only message.
trap 'rm -rf "$_tmp" 2>/dev/null || true' EXIT
export HOME="$_tmp/home" XDG_CONFIG_HOME="$_tmp/config" XDG_STATE_HOME="$_tmp/state"
export GIT_CONFIG_GLOBAL="$_tmp/gitconfig" GIT_CONFIG_SYSTEM=/dev/null
# Determinism freeze (crates/thegn-host/src/e2e_freeze.rs): pins stats, the
# clock, the version wordmark, the activity FSM and the media badge, so text
# and pixel snapshots are byte-stable across runs and machines.
export THEGN_E2E=1
# Panes run a plain `sh` with a fixed `$ ` prompt: the developer's $SHELL and
# prompt (user@host, cwd — also mirrored into the pane title and the sidebar
# row label) would make every pane-bearing snapshot machine-specific. A wrapper
# rather than $ENV because thegn's curated pane env doesn't carry ENV/PS1.
mkdir -p "$_tmp/bin"
printf '#!/bin/sh\nexport PS1="$ " PROMPT_COMMAND=\nexec /bin/sh --norc --noprofile -i\n' > "$_tmp/bin/e2esh"
chmod +x "$_tmp/bin/e2esh"
export SHELL="$_tmp/bin/e2esh"
# Cut the session D-Bus: otherwise the developer's live media player leaks
# into the statusbar/masthead media badge, flapping the text the specs match.
export DBUS_SESSION_BUS_ADDRESS="unix:path=/dev/null/e2e-no-dbus"
# In-process panes: muse kills compositors without a quit path — daemon-backed
# panes would detach into never-reaped sessions (a leaked daemon + shell per
# case), and the async "persist" chip would flake the statusbar.
# The runtime dir is isolated for the same reason smoke isolates it: the
# daemon socket path prefers $XDG_RUNTIME_DIR.
export THEGN_NO_DAEMON=1 XDG_RUNTIME_DIR="$_tmp/run"
# Start in the normal UI, not the first-run onboarding wizard / keymap picker —
# those modals would swallow the specs' driven keystrokes (fresh per-case DB).
export THEGN_SKIP_ONBOARDING=1
mkdir -p "$HOME" "$XDG_CONFIG_HOME/thegn" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR"
printf '[user]\nname = e2e\nemail = e2e@example.invalid\n' > "$_tmp/gitconfig"
# Run panes on the host (no container): this suite exercises thegn's UI, not the
# sandbox runtime (that is sandbox-e2e-*). A container backend would also fail
# to reach the cut session bus and log a pane-crash ERROR the guard rejects.
# Media is off (THEGN_E2E forces it too): the player watcher reaches the
# session bus / playerctl even with DBUS_SESSION_BUS_ADDRESS cut.
printf '[sandbox]\nbackend = "none"\n[media]\nenabled = false\n' > "$XDG_CONFIG_HOME/thegn/config.toml"
# The same env-level override 30-lsp.yaml documents for its XDG_CONFIG_HOME
# bypass, so a spec that re-points config cannot silently re-enable podman.
export THEGN_SANDBOX_BACKEND=none
export E2E_RESULTS="$(pwd)/e2e-results"
rm -rf "$E2E_RESULTS"
'''

# Show available recipes (default).
default:
    @just --list

# --- build / package ------------------------------------------------------

# Debug build (the whole cargo workspace: core, svc, host).
build:
    cargo build --workspace
    # Keep the dev release channel compiling (the host `dev` feature flips the
    # default channel; empty feature, so this is a cheap incremental check).
    cargo check -p thegn-host --features dev

# Fast inner-loop check: typecheck + clippy on lib/bin code only (no test/bench
# targets, no tests, no coverage). Pass a crate to scope it further, e.g.
# `just quick thegn-host`. Use this while iterating; run the heavy gates
# (`just test` / `just coverage` / `just ci`) only when preparing to push/PR.
quick pkg="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "{{pkg}}" ]; then scope="-p {{pkg}}"; else scope="--workspace"; fi
    cargo clippy $scope -- -D warnings

# Cross-platform regression gate: typecheck per-OS code for macOS + Windows on
# this box. Catches the #1 cross-platform breakage — won't-compile — without
# needing macOS/Windows runners.
#
# macOS coverage is every crate that builds WITHOUT a darwin cross C toolchain
# (there isn't one here): the leaf crates plus the gtui/tg-kit family. It stops
# at `thegn-core`/`-svc`/`-host`/`gtui-query`, whose build scripts (`ring`,
# bundled sqlite, libgit2) compile C for the target. Those are covered by the
# opt-in `macos` CI job and the on-device checklist in CONTRIBUTING.
#
# Windows checks the WHOLE workspace (the native-Windows port): windows-gnu
# shares `cfg(windows)` with -msvc, so this catches any newly ungated unix API
# use; the C build scripts use the mingw-w64 cc the dev shells wire via
# CC_x86_64_pc_windows_gnu. The msvc truth gate is the opt-in `windows` CI job.
#
# EACH LEG SKIPS LOUDLY when its toolchain is absent rather than failing, so the
# recipe (and therefore `just ci`) is runnable on a Mac, where there is no mingw
# cross-cc. On CI both legs are present, so nothing is silently skipped there.
check-cross:
    #!/usr/bin/env bash
    set -euo pipefail
    rustlib="$(rustc --print sysroot)/lib/rustlib"
    # The C-dep-free leaves: no build script compiles C for the target, so these
    # typecheck against any target with rust-std, no cross cc required. They also
    # carry most of the per-OS code (sysinfo metrics; MPRIS/SMTC/AppleScript
    # media). thegn-core/-svc/-host + gtui-query are NOT here — ring, bundled
    # sqlite and libgit2 make them need a real cross cc.
    leaves="thegn-metrics thegn-media tg-kit gtui-core gtui-render gtui-app"
    if [ -d "$rustlib/aarch64-apple-darwin" ]; then
      for crate in $leaves; do
        cargo check -p "$crate" --target aarch64-apple-darwin
      done
    else
      echo "check-cross: SKIP aarch64-apple-darwin — no rust-std for that target." >&2
      echo "  Use 'nix develop' — the flake toolchain declares it." >&2
    fi
    if [ ! -d "$rustlib/x86_64-pc-windows-gnu" ]; then
      echo "check-cross: SKIP x86_64-pc-windows-gnu — no rust-std for that target." >&2
      echo "  Use 'nix develop' — the flake toolchain declares it." >&2
    elif [ -n "${CC_x86_64_pc_windows_gnu:-}" ]; then
      cargo check --workspace --target x86_64-pc-windows-gnu
    else
      # No mingw-w64 cross cc — expected off Linux, where flake.nix gates it.
      # Partial cover beats none: the leaves still typecheck, so a Windows break
      # in the per-OS media/metrics code stays visible from a Mac. Say plainly
      # what is NOT covered rather than reporting a silent pass.
      echo "check-cross: x86_64-pc-windows-gnu — no mingw-w64 cross cc; checking leaves only." >&2
      echo "  NOT covered: thegn-core, thegn-svc, thegn-host, gtui-query (need a cross cc)." >&2
      for crate in $leaves; do
        cargo check -p "$crate" --target x86_64-pc-windows-gnu
      done
    fi

# Debug build of the host with the in-process sampling profiler compiled in
# (the `profiling` feature → SIGUSR2 flamegraph capture). Same artifact path as
# `build` (target/debug/thegn), so `start-term` picks it up transparently.
build-profiling:
    cargo build --features profiling -p thegn-host

# Release build (the whole cargo workspace).
release:
    cargo build --workspace --release

# Build a static x86_64-linux-musl `thegn` — the resident bridge binary pushed
# into Firecracker provider envs (Sprites). Self-contained (musl + bundled
# sqlite + rustls, no openssl) so it runs in a bare microVM. Needs the musl
# target (`rustup target add x86_64-unknown-linux-musl`) + a musl cross cc; in
# nix use `nix build .#thegn-musl` instead. Output:
# target/x86_64-unknown-linux-musl/release/thegn — point THEGN_BRIDGE_BINARY
# at it (or drop it next to the host exe as `thegn-musl`).
build-musl:
    cargo build --release -p thegn-host --bin thegn --target x86_64-unknown-linux-musl

# Build the resident musl bridge (hermetically, via nix — no host musl toolchain
# needed) and drop it next to BOTH the debug and release host binaries as
# `thegn-musl`, where `bridge_sup::bridge_binary_path()` auto-discovers it. Without
# it, `bridge_binary_path()` is None, the bridge is never pushed to provider envs,
# and every reverse tunnel (nix cache :8484) execs a missing binary →
# in-sandbox `:8484 could not connect` + slow from-source devShell builds. Run once
# (nix caches it, so unchanged rebuilds are instant); re-run after source changes,
# then restart the instance so the fresh bridge is pushed.
bridge:
    nix build .#thegn-musl -o result-bridge
    mkdir -p target/debug target/release
    install -m755 result-bridge/bin/thegn target/debug/thegn-musl
    install -m755 result-bridge/bin/thegn target/release/thegn-musl
    # Strip the installed copies — the bridge is pushed byte-for-byte over the exec
    # stream into each fresh env, so shedding the (runtime-unneeded) symbol/debug
    # tables cuts ~20% off every push. The nix store artifact stays intact.
    strip target/debug/thegn-musl target/release/thegn-musl
    @echo "bridge: installed → target/{debug,release}/thegn-musl ($(du -h target/release/thegn-musl | cut -f1), stripped)"

# Run the native host compositor. Builds it first. Run from a real terminal —
# it acquires raw mode and owns the screen.
host *args: build
    {{bin}} {{args}}

# Regenerate the help allowlists from the current state: test/help-ratchet.txt
# (actions no page claims) and test/help-prose-ratchet.txt (actions a page
# claims but never writes about). The ratchet tests only let these files
# shrink; run after documenting actions in docs/help/ to lock in the win.
help-ratchet-update:
    THEGN_HELP_RATCHET_UPDATE=1 cargo test -p thegn-host help_ratchet_update -- --ignored
    THEGN_HELP_RATCHET_UPDATE=1 cargo test -p thegn-host help_prose_ratchet_update -- --ignored

# Regenerate every architecture ratchet allowlist (test/*-ratchet.txt) from the
# current tree, headers preserved. Use after paying debt down; never to add
# debt (the lists are shrink-only — review the diff).
ratchet-update:
    THEGN_RATCHET_UPDATE=1 cargo test -p thegn-host ratchet
    THEGN_RATCHET_UPDATE=1 cargo test -p thegn-core platform_ratchet
    THEGN_RATCHET_UPDATE=1 cargo test -p thegn-svc platform_ratchet
    THEGN_RATCHET_UPDATE=1 cargo test -p thegn-media platform_ratchet
    THEGN_RATCHET_UPDATE=1 cargo test -p thegn-metrics platform_ratchet
    RATCHET_UPDATE=1 bash test/ratchet.sh forge-leak 'thegn_core::github::|use thegn_core::github|Command::new\("gh"\)' crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src
    RATCHET_UPDATE=1 bash test/ratchet.sh async-trait '#\[allow\(async_fn_in_trait\)\]' crates
    RATCHET_UPDATE=1 bash test/ratchet.sh ignored-result 'let _ = |\.ok\(\);' crates
    RATCHET_UPDATE=1 bash test/ratchet.sh json-emit 'serde_json::to_string(_pretty)?\(' crates/thegn-host/src/cmd ':!crates/thegn-host/src/cmd/mod.rs'
    RATCHET_UPDATE=1 bash test/ratchet.sh element 'draw_text\(' crates/thegn-host/src ':!crates/thegn-host/src/logotype.rs' ':!crates/thegn-host/src/loading/screen.rs' ':!crates/thegn-host/src/chrome_tests.rs'
    THEGN_RATCHET_UPDATE=1 cargo test -p thegn-core --test env_overlay_coverage
    THEGN_RATCHET_UPDATE=1 cargo test -p thegn-core surface_gaps_ratchet_update -- --ignored

# Startup benchmarks (hyperfine; needs the dev shell). Not part of `just ci` —
# timings are machine-dependent. Three numbers: process/clap baseline; cold
# launch → first diff-flushed frame (fresh state: pays schema creation + first
# seed, i.e. the once-per-machine path); warm launch → first frame (existing
# state: the daily path). termwiz needs a PTY, so wrap in `script`, which adds
# a small constant overhead — fine for A/B deltas. Isolated XDG_STATE_HOME so
# the bench never touches the daily DB. The `script` wrapper goes through
# `test/lib/pty.sh` because its CLI differs between util-linux and BSD/macOS.
bench: release
    #!/usr/bin/env bash
    set -euo pipefail
    # shellcheck source=test/lib/pty.sh disable=SC1091
    source test/lib/pty.sh
    launch="env XDG_STATE_HOME=/tmp/tg-bench-state THEGN_NO_MIGRATE=1 THEGN_BENCH_FIRST_FRAME_EXIT=1 target/release/thegn"
    hyperfine --warmup 3 'target/release/thegn --version'
    hyperfine --warmup 3 --prepare 'rm -rf /tmp/tg-bench-state' "$(pty_cmd "$launch")"
    hyperfine --warmup 3 "$(pty_cmd "$launch")"

# Guard run by every perf recipe: refuse to measure a debug or stale binary.
# The debug-vs-release CPU gap is ~2.5x (and cargo test/clippy don't rebuild
# target/debug/thegn), so a perf number from the wrong binary is worse than
# none. Prints the resolved binary + mtime + profile so reports self-describe.
_perf-guard:
    #!/usr/bin/env bash
    set -euo pipefail
    b="target/release/thegn"
    if [ ! -x "$b" ]; then
      echo "perf: $b not built — run 'just release' first" >&2; exit 1
    fi
    src_newest="$(find crates apps -name '*.rs' -newer "$b" 2>/dev/null | head -1 || true)"
    if [ -n "$src_newest" ]; then
      echo "perf: $b is STALE (newer source: $src_newest) — run 'just release'" >&2; exit 1
    fi
    echo "perf: binary=$b mtime=$(date -r "$b" '+%F %T') profile=release"

# Idle CPU benchmark: launch thegn in a PTY over a fixture of N worktrees, let
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

# Switch/input latency under a multi-pane output flood (A/B only — advisory,
# machine-dependent). Reads switch_p99/input_p99 from the perf rollup while
# several worktree shells scroll at full speed and Alt+Down fires mid-flood.
perf-flood *args: release _perf-guard
    bash test/perf/flood.sh {{args}}

# Workspace-switch (T3) latency: registers several fixture repos as
# workspaces, then bursts Shift+Alt+Down around the ring and reads
# switch_ws_p99/render_full_p99 from the perf rollup (A/B only — advisory).
perf-t3 *args: release _perf-guard
    bash test/perf/t3-workspace-switch.sh {{args}}

# Criterion micro-benchmarks across the workspace (hot git path, core models).
# `cargo bench` uses the release-grade bench profile. For a debug-vs-release
# A/B, append `--profile dev`. Pass extra criterion args after `--`.
bench-micro *args:
    cargo bench --workspace {{args}}

# Just the git hot-path benches (is_dirty / ahead_behind / current_branch,
# gix vs CLI, scaled by worktree count) — the dominant idle cost.
bench-micro-svc *args:
    cargo bench -p thegn-svc --bench git_hot {{args}}

# Umbrella: startup (hyperfine) + idle CPU + micro-benches. Self-describing
# (each sub-recipe prints its binary/profile). Machine-dependent — not in CI.
perf: bench bench-idle bench-micro
    @echo "perf: startup + idle + micro complete"

# Build thegn with the in-process sampling profiler (release + profiling
# feature). SIGUSR2 toggles a flamegraph capture written to
# $XDG_STATE_HOME/thegn/profiles/. Profiles the live process (sidesteps
# ptrace_scope=1, which blocks external perf/gdb attach).
release-profiling:
    cargo build --release --features profiling -p thegn-host

# Launch thegn under the profiler and print how to drive it. Run from a real
# terminal. `kill -USR2 <pid>` once to start sampling, again to dump.
profile *args: release-profiling
    #!/usr/bin/env bash
    set -euo pipefail
    echo "profiler: send 'kill -USR2 \$(pgrep -n thegn)' to start, again to dump."
    echo "profiles land in \$XDG_STATE_HOME/thegn/profiles/ (or ~/.local/state/...)."
    THEGN_LOG=thegn::perf=info target/release/thegn {{args}}

# Build the Nix package; symlinks ./result.
nix-build:
    nix build .#thegn-nobridge

# The full default install — host binary plus the adjacent static-musl bridge,
# i.e. exactly what `nix profile install github:blakeashleyjr/thegn` produces.
# On x86_64-linux that is TWO release compiles of the workspace (native +
# cross-to-musl), so it is roughly double `nix-build` and is kept out of the
# routine gate. Run it before cutting a release, or via a CI dispatch.
nix-build-full:
    nix build .#default

# Print the store path without creating ./result.
path:
    @nix build .#default --no-link --print-out-paths

# Evaluate all flake outputs.
flake-check:
    nix flake check

# --- spec-driven development (OpenSpec) -----------------------------------
# thegn manages its OWN development with OpenSpec (see openspec/, CLAUDE.md).
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

# The full gate. `lint` now runs the treefmt fail-on-change check first, so the
# formatting gate lives there (no separate `fmt-check` stage needed here).
ci: lint deps-audit build check-cross check-features check-msrv test test-doc doc-check openspec-validate coverage smoke sandbox-e2e-dns sandbox-e2e-db term-check nix-build
    @echo "ci: all green"

# The local superset: everything `ci` gates plus the muse e2e suite, which is
# opt-in in CI (`[ci-e2e]`) until its timeout is fixed and baselines are
# re-recorded — so `ci` itself stays green-able on a clean checkout.
ci-local: ci e2e
    @echo "ci-local: all green"

# Feature matrix: every named feature compiles alone and all together, so a
# feature nobody enables by default (`control-grpc`, `test-utils`, `profiling`,
# `standalone`) can't rot. `dev` is covered by `build`.
check-features:
    cargo check --workspace --all-features
    cargo check -p thegn-svc --features control-grpc
    cargo check -p thegn-core --features test-utils
    cargo check -p thegn-host --features profiling
    cargo check -p tg-kit --features standalone

# The declared MSRV (`rust-version` in Cargo.toml) actually builds. `cargo-1.89`
# is the flake's pinned MSRV toolchain (msrvRustToolchain) — bump both together.
check-msrv:
    @command -v cargo-1.89 >/dev/null 2>&1 || { echo "check-msrv: 'cargo-1.89' not found — run inside 'nix develop' (or 'direnv allow')"; exit 1; }
    cargo-1.89 check --workspace --locked

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

# Semantic + snapshot + crash e2e gate: run every muse spec against a live thegn
# binary in a real PTY. Specs assert on stable UI text (`expect_visible` /
# `expect_count` / `expect_not_visible` / `expect_style`), diff text/styled
# snapshots against the committed baselines in test/muse/snapshots/ (byte-
# stable thanks to the THEGN_E2E freeze — see _e2e_env), and end with a
# `check_file` guard that fails on any panic / overflow / corruption in the log
# (panics reach the log through the hook `thegn_core::log_trace` installs).
# `--ci` makes a missing baseline a failure: add one deliberately with
# `just e2e-update`, never by accident. A failing case leaves e2e-results/<case>/
# with final.txt/.png, per-snapshot actual/diff/baseline files and a trace.
# thegn is put on PATH so specs can use spawn: ["thegn"] portably.
#
# The suite is hermetic w.r.t. the developer's environment: `_e2e_env` isolates
# HOME, the XDG dirs, and git config into a throwaway temp dir (cleaned on exit),
# so warm/shared envs can neither change behavior nor leak test state. Each spec
# additionally isolates XDG_STATE_HOME per case via `case_tmp_env`.
e2e: build
    #!/usr/bin/env bash
    {{_e2e_env}}
    # muse takes spec FILES (a bare directory is "Is a directory" — os error 21).
    # One worker: the stress specs fire resize/chord storms whose timing is
    # the point; a second compositor competing for the CPU turned those into
    # load-dependent flakes (and masked a real race, tasks.md 748). ~8 min.
    PATH="$(pwd)/target/debug:$PATH" muse run test/muse/specs/*.yaml \
        --reporter pretty --workers 1 --deadline-ms 20000 --case-timeout-ms 180000 \
        --ci --snapshots-dir test/muse/snapshots --artifacts "$E2E_RESULTS"

# Re-record the snapshot baselines (after an intentional UI change). Review the
# diff under test/muse/snapshots/ before committing it.
e2e-update: build
    #!/usr/bin/env bash
    {{_e2e_env}}
    PATH="$(pwd)/target/debug:$PATH" muse run test/muse/specs/*.yaml \
        --reporter pretty --workers 1 --deadline-ms 20000 --case-timeout-ms 180000 \
        --update-snapshots --snapshots-dir test/muse/snapshots --artifacts "$E2E_RESULTS"

# Run only the glitch-hunt specs (18–28) — the boundary/stress subset.
e2e-glitch: build
    #!/usr/bin/env bash
    {{_e2e_env}}
    PATH="$(pwd)/target/debug:$PATH" muse run \
        test/muse/specs/1[89]-*.yaml test/muse/specs/2[0-8]-*.yaml \
        --reporter pretty --workers 1 --deadline-ms 20000 --case-timeout-ms 180000 \
        --ci --snapshots-dir test/muse/snapshots --artifacts "$E2E_RESULTS"

# (e2e/stress/perf harnesses drove the old zellij CLI's worktree-creation
# commands headlessly; worktree/workspace/pin creation is now an interactive
# compositor action, exercised by the host's unit tests.)

# The gate covers the testable core only (crate `thegn-core`). EXCLUDED: the
# exec / exit / subprocess seams that can't be unit-covered without real external
# tools (git/gh/podman/ssh) — exercised by smoke instead. See docs/coverage.md.
# Everything NOT matched here (config, db, theme, diff_highlight, models) is gated
# at 95% lines. The native host and the svc layer carry their own tests but are
# not part of this gate (their I/O-heavy surface is the same reason the seams
# above are excluded).
cov_ignore := 'thegn-core/src/(repo|worktree|sandbox|sandbox_mounts|sandbox_preflight|sandbox_prefetch|remote|github|picker|util|msg|out|log|devenv|direnv|profile)\.rs'

# Coverage gate: core ≥95% lines. Writes lcov to target/coverage.
coverage:
    mkdir -p target/coverage
    # Discard any stale .profraw from earlier instrumented runs — merging them
    # produces a false-low (or false-high) line %, which can spuriously fail the
    # gate locally (CI's clean checkout never sees this).
    cargo llvm-cov clean --workspace
    cargo llvm-cov -p thegn-core --lib --fail-under-lines 95 \
      --ignore-filename-regex '{{cov_ignore}}' \
      --lcov --output-path target/coverage/lcov.info
    @echo "coverage: core ≥95% lines"

# Coverage as a browsable HTML report (target/llvm-cov/html).
coverage-html:
    cargo llvm-cov -p thegn-core --lib --html \
      --ignore-filename-regex '{{cov_ignore}}'

# --- quality --------------------------------------------------------------

# Comprehensive linting: rust (clippy), bash (shellcheck), yaml (yamllint), toml (taplo).
lint:
    @for t in treefmt shellcheck yamllint taplo; do command -v "$t" >/dev/null 2>&1 || { echo "lint: '$t' not found — run inside 'nix develop' (or 'direnv allow'); 'just doctor' for details"; exit 1; }; done
    # Formatting gate (treefmt, fail-on-change) — FIRST so drift fails fast before
    # the clippy compile. This is what makes `just lint` (and thus the merge-queue
    # `gate_command`) reject unformatted code: the fold-actor lands via plumbing
    # commits, so git's pre-commit/treefmt hook never fires on that path. `--ci`
    # formats in place then exits nonzero on any change (mirrors `just fmt-check`).
    treefmt --ci
    cargo clippy --workspace --all-targets -- -D warnings
    # Every tracked shell script, not a hand-kept list — the list had drifted to
    # 9 of 20 files, silently excluding the user-facing setup-macos.sh and all
    # of scripts/ci/. `git ls-files` keeps new scripts covered by default.
    git ls-files -z '*.sh' | xargs -0 shellcheck -x
    yamllint .
    # Tracked TOML only. Bare `taplo lint` walks the whole cwd and was linting
    # .direnv/flake-inputs (i.e. nixpkgs) and target/ — 122 files, almost none
    # of them ours.
    git ls-files -z '*.toml' | xargs -0 taplo lint
    # Guardrail: all git must route through util::git_cmd / GitLoc so GIT_ENV_VARS
    # is scrubbed (the core.worktree-pollution class). Only the builder in util.rs
    # may call `git` directly; raw `Command::new("git")` anywhere else is rejected.
    # Comment lines are ignored (doc-comments legitimately name the pattern they forbid).
    ! grep -rIn 'Command::new("git")' crates --include='*.rs' | grep -v 'thegn-core/src/util.rs' | grep -vE ':[0-9]+:[[:space:]]*//' || (echo 'ERROR: raw Command::new("git") outside util::git_cmd — route through git_cmd/GitLoc to scrub GIT_ENV_VARS' && exit 1)
    # Guardrail: pre-rename brand tokens must not come back — this is thegn.
    # Token list + allowlist live in the script. See test/brand-guard.sh.
    bash test/brand-guard.sh
    # Guardrail: stale architecture claims (old emulator name, never-landed ssh crate,
    # removed per-file size limit, "e2e runs every push") must not come back.
    bash test/stale-docs-guard.sh
    # Architecture ratchets (shrink-only allowlists; test/*-ratchet.txt headers
    # explain each rule). The Rust-side ones run in `just test`.
    bash test/ratchet.sh forge-leak 'thegn_core::github::|use thegn_core::github|Command::new\("gh"\)' crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src
    bash test/ratchet.sh async-trait '#\[allow\(async_fn_in_trait\)\]' crates
    bash test/ratchet.sh ignored-result 'let _ = |\.ok\(\);' crates
    bash test/ratchet.sh json-emit 'serde_json::to_string(_pretty)?\(' crates/thegn-host/src/cmd ':!crates/thegn-host/src/cmd/mod.rs'
    # Element contract: no NEW interactive chrome painted with raw `draw_text` +
    # a hand-built hit table — build it through `crate::element` instead (see
    # test/element-ratchet.txt for the rule + burn-down).
    bash test/ratchet.sh element 'draw_text\(' crates/thegn-host/src ':!crates/thegn-host/src/logotype.rs' ':!crates/thegn-host/src/loading/screen.rs' ':!crates/thegn-host/src/chrome_tests.rs'
    # NOTE: the host-key policy-chokepoint ratchet (THE-66) is enforced as a Rust
    # test in each crate's `platform_ratchet_tests.rs`
    # (`host_key_literals_stay_in_the_chokepoint`, allowlists
    # test/hostkey-{core,svc,host}-ratchet.txt), so it runs in `just test` — the
    # pre-push gate — not here. `just ratchet-update` regenerates it via the
    # `cargo test -p <crate> platform_ratchet` lines above.
    # Guardrail: the git read engine is config-selected — host code takes it from
    # `git_handle::get()`, never constructs `GixGit` itself (writes use `CliGit`
    # explicitly, by design).
    ! grep -rIn 'GixGit::new()' crates/thegn-host/src --include='*.rs' | grep -vE ':[0-9]+:[[:space:]]*//' || (echo 'ERROR: GixGit constructed in the host — use crate::git_handle::get()' && exit 1)
    # Guardrail: the idle loop never polls. Every `poll_input(` in the host is
    # either a zero-timeout drain, the attach client's blocking `None`, or THE
    # one timed site that consumes `idle_poll::poll_timeout` (tested pure).
    ! grep -rIn 'poll_input(' crates/thegn-host/src --include='*.rs' | grep -vE ':[0-9]+:[[:space:]]*//' | grep -vE 'poll_input\(None\)|Duration::ZERO\)|poll_input\(timeout\)' || (echo 'ERROR: a timed poll_input outside idle_poll::poll_timeout — the idle loop must never poll (CLAUDE.md)' && exit 1)
    test "$(grep -rIn 'poll_input(timeout)' crates/thegn-host/src --include='*.rs' | grep -vE ':[0-9]+:[[:space:]]*//' | wc -l)" = 1 || (echo 'ERROR: expected exactly one poll_input(timeout) site (run.rs)' && exit 1)

# Repair a wedged checkout: strip a stray `core.worktree` that an external
# worktree tool (herdr) or a GIT_*-exporting child leaked into the shared
# `.git/config`. Symptom: `git add`/`commit`/`status` mis-target another tree,
# or (once the leaked path is deleted) git aborts with "Invalid path" / "must be
# run in a work tree". Pure-text repair — needs no working git, so it fixes the
# case a pre-commit hook can't (git dies before hooks run). Same key thegn heals
# in-process at startup + on worktree switch; this covers manual/CI git. No-op
# when clean.
heal-git:
    sh test/git-hooks/heal-worktree.sh -v || true
    @top=$(git rev-parse --show-toplevel 2>/dev/null) && echo "heal-git: ok — worktree $top" || echo "heal-git: git still wedged — inspect .git/config by hand"

# Diagnose the dev environment: report any missing toolchain bit with a one-line
# fix. Exits non-zero if anything is missing — handy for agents/CI to confirm the
# gates won't silently skip. (thegn panes get the devShell automatically via
# `[sandbox] inject_devshell`; this is for working ON thegn directly.)
doctor:
    #!/usr/bin/env bash
    set -uo pipefail
    echo "thegn dev-env doctor"
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
# Rustdoc gate. `--document-private-items` is load-bearing, not cosmetic: this
# codebase is overwhelmingly private/pub(crate), so without it rustdoc never
# looks at most of the doc comments and broken intra-doc links rot unseen (six
# were hiding behind exactly that gap when this flag was added).
doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --document-private-items

# Format everything via treefmt (rust, nix, bash, toml, yaml, markdown).
fmt:
    nix fmt

# Check formatting without writing (CI-friendly).
fmt-check:
    nix fmt -- --ci

# Unit tests. cargo-nextest runs the suite with better parallelism than
# `cargo test`. This recipe is the single source of truth shared by the CI
# `test` job and the pre-push hook. Doctests are `test-doc` (CI-only) — see
# the note there.
test:
    cargo nextest run --workspace

# Doctest pass. Split out of `test` (and therefore off pre-push) because it is
# a THIRD full-workspace compile, on top of clippy's and nextest's, and this
# repo has ZERO runnable doctests to show for it: every one of the ~10 doc
# fences is ```text / ```ignore / ```sh (architecture diagrams and shell
# recipes, not assertions). It stays in `just ci` so a genuinely runnable
# doctest added later is still compiled and run before a release.
test-doc:
    cargo test --doc --workspace

# Formal verification (bounded model checking, CBMC via Kani) of the pure
# color-quantization math in `thegn-core::termcaps` (the `#[cfg(kani)]`
# proofs). Opt-in and machine-local: needs a one-time `cargo install --locked
# kani-verifier && cargo kani setup`. Deliberately NOT part of `just ci` — Kani's
# bundled CBMC toolchain is non-hermetic and the solve is slow.
#
# KNOWN BLOCKER (spike finding, 2026-07-07, kani 0.67.0): this does not currently
# compile. Kani must build all of `thegn-core`, and its transitive dep
# `libsqlite3-sys` (via `rusqlite`) uses the unstable `cfg_select!` in its build
# script, which Kani's pinned `nightly-2025-11-21` rejects. The 5 harnesses WERE
# verified (all SUCCESSFUL, ~1.2s) by extracting the color fns verbatim into a
# standalone dep-free crate. To run in-tree, either Kani's toolchain must advance
# past that dep, or the pure-math module must be split into a leaf crate with no
# heavy deps. Until then, treat the `#[cfg(kani)]` proofs as documentation.
verify-kani:
    cargo kani -p thegn-core

# Live integration tests against the REAL Sprites API (creates + destroys throwaway
# sprites — real cloud spend). Sources SPRITES_TOKEN from .envrc.local. Validates
# the provider exec/fs/checkpoint primitives + the env-provisioning clone path that
# back the transparent sandbox/remote feature. `#[ignore]` so normal `just test`
# skips them.
test-sprite:
    [ -f .envrc.local ] && set -a && . ./.envrc.local && set +a; \
      [ -n "${SPRITES_TOKEN:-}" ] || { echo "SPRITES_TOKEN not set (put it in .envrc.local)" >&2; exit 1; }; \
      cargo test -p thegn-svc --test sprites_live -- --ignored --nocapture

# Live sprite-recycle verification (hosts-as-resources S1/S2): checkpoint
# capture, stale restore-in-place, claimed-delete round trip, bad-checkpoint
# fallback. Real cloud spend; serial (the tests hold the crate env lock).
sprites-live-recycle:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -f .envrc.local ] && . ./.envrc.local
    [ -n "${SPRITES_TOKEN:-}" ] || { echo "SPRITES_TOKEN not set (put it in .envrc.local)" >&2; exit 1; }
    cargo test -p thegn-host --bin thegn live_recycle -- --ignored --nocapture --test-threads=1

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
      PODMAN_E2E_FORCE=1 cargo test -p thegn-core -- sandbox; \
    elif [ "$$PODMAN_E2E_FORCE" = "1" ]; then \
      echo "sandbox-e2e: PODMAN_E2E_FORCE=1 but podman not found"; exit 1; \
    else \
      echo "sandbox-e2e: podman not found, skipping (set PODMAN_E2E_FORCE=1 to fail on missing)"; \
    fi

# DNS filter E2E — Tier 1, no podman needed; always runs in CI.
sandbox-e2e-dns:
    cargo test -p thegn-core --test sandbox_dns_e2e

# DB audit trail — Tier 1, no podman; always runs in CI.
sandbox-e2e-db:
    cargo test -p thegn-core --lib -- db::tests::container_events

# Full podman-backed suite (Tier 2). Discovers podman and exits cleanly if absent.
sandbox-e2e-full: build
    @if command -v podman >/dev/null 2>&1; then \
      echo "sandbox-e2e-full: podman found, running Tier 2 tests"; \
      PODMAN_E2E_FORCE=1 cargo test -p thegn-core \
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
    ./test/smoke.sh "$(nix build .#default --no-link --print-out-paths)/bin/thegn"

# --- run / install --------------------------------------------------------

# Run a subcommand against the debug build, e.g. `just run list`.
run *args: build
    env THEGN_NO_MIGRATE=1 {{bin}} {{args}}

# Build and run the native host locally in an isolated state root.
start name="dev": build
    state="$HOME/.thegn-{{name}}/state"; run="$HOME/.thegn-{{name}}/run"; pidfile="$run/thegn.pid"; mkdir -p "$state" "$run"; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      echo $$ > "$pidfile"; exec env \
      "THEGN_ALACRITTY_CONFIG=$PWD/config/alacritty.toml" \
      "XDG_STATE_HOME=$state" \
      "XDG_RUNTIME_DIR=$run" \
      "THEGN_NO_MIGRATE=1" \
      {{bin}}

# Alias for `start`.
attach: start

# Like `start`, but every invocation spins up a FRESH, randomly-named sandbox
# ($HOME/.thegn-clean-<rand>/{state,run}) so it never touches the daily DB or
# any other `just start`/install instance. Nothing is reattached (unique name),
# so you always get a clean db + work area. Ephemeral by design — the dirs are
# left on disk for post-mortem; sweep them with `just install-clean-sweep`.
install-clean: build
    #!/usr/bin/env bash
    set -euo pipefail
    rand="$(od -An -N6 -tx1 /dev/urandom | tr -d ' ')"
    root="$HOME/.thegn-clean-$rand"; state="$root/state"; run="$root/run"
    mkdir -p "$state" "$run"
    echo "install-clean: fresh sandbox at $root (XDG_STATE_HOME=$state)" >&2
    exec env \
      "THEGN_ALACRITTY_CONFIG=$PWD/config/alacritty.toml" \
      "XDG_STATE_HOME=$state" \
      "XDG_RUNTIME_DIR=$run" \
      "THEGN_NO_MIGRATE=1" \
      "{{bin}}"

# Remove every ephemeral `just install-clean` sandbox ($HOME/.thegn-clean-*).
install-clean-sweep:
    rm -rf "$HOME"/.thegn-clean-*; echo "swept $HOME/.thegn-clean-*"

# Like `start`, but in the dev release channel (experimental subsystems enabled)
# via the runtime override — no separate binary needed. `just start-dev name=x`.
start-dev name="dev": build
    state="$HOME/.thegn-{{name}}/state"; run="$HOME/.thegn-{{name}}/run"; pidfile="$run/thegn.pid"; mkdir -p "$state" "$run"; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      echo $$ > "$pidfile"; exec env \
      "THEGN_ALACRITTY_CONFIG=$PWD/config/alacritty.toml" \
      "XDG_STATE_HOME=$state" \
      "XDG_RUNTIME_DIR=$run" \
      "THEGN_NO_MIGRATE=1" \
      "THEGN_CHANNEL=dev" \
      {{bin}}

# Headless terminal-capability matrix: run `thegn doctor` under a set of
# degraded environments (each a clean `env -i`, so the outer terminal's
# COLORTERM / TERM_PROGRAM can't leak in and mask a degradation) and assert the
# resolved color depth + glyph level match what each terminal should get. Proves
# the graceful-degradation layer (`thegn_core::termcaps`) end to end without a
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
    echo "terminal-capability matrix (thegn doctor, clean env):"
    check kitty      truecolor  full  TERM=xterm-kitty COLORTERM=truecolor LANG=en_US.UTF-8
    check bare       16-color   ascii TERM=xterm LANG=C
    check no-color   monochrome full  TERM=xterm-kitty COLORTERM=truecolor NO_COLOR=1 LANG=en_US.UTF-8
    check 256color   256-color  basic TERM=xterm-256color LANG=en_US.UTF-8
    check glyph=asci truecolor  ascii TERM=xterm-kitty COLORTERM=truecolor LANG=en_US.UTF-8 THEGN_THEME_GLYPHS=ascii
    check color=16   16-color   full  TERM=xterm-kitty COLORTERM=truecolor LANG=en_US.UTF-8 THEGN_THEME_COLOR=16
    if [ "$fail" = 0 ]; then echo "term-check: all green"; else echo "term-check: MISMATCH"; exit 1; fi

# Point THIS repo (all its worktrees) at a sandbox backend / managed-sandbox
# provider, without touching any other repo — the engine behind the `backend=`
# param on `start-term`/`start-term-release`. Writes a per-repo `.thegn.toml`
# overlay at the MAIN worktree (resolved via `git --git-common-dir`, mirroring
# `thegn_core::repo::main_worktree`), which every thegn worktree reads.
#   - `sprites` (a managed-sandbox PROVIDER) → overlay `env = "sprites"`, and the
#     global `[env.sprites]` block is auto-scaffolded into
#     $XDG_CONFIG_HOME/thegn/config.toml if missing. Needs SPRITES_TOKEN set
#     (fail-fast); the `sprite` CLI is only advisory now — the default exec=auto
#     attaches the pane over the native WSS exec API with no vendor CLI.
#   - a real sandbox backend (podman|docker|bwrap|systemd|none) → overlay
#     `[sandbox] backend = "<x>"`.
# Empty `backend` is a no-op. Refuses to clobber a hand-authored overlay that
# already carries a `[keybinds]`/`[sandbox]` table. To revert: delete the overlay
# (`rm "$(git rev-parse --show-toplevel)/.thegn.toml"` from the main checkout).
_apply-backend backend="":
    backend="{{backend}}"; \
    if [ -n "$backend" ]; then \
      root="$(git rev-parse --path-format=absolute --git-common-dir)"; \
      case "$root" in */.git) root="${root%/.git}";; esac; \
      overlay="$root/.thegn.toml"; \
      cfg="${XDG_CONFIG_HOME:-$HOME/.config}/thegn/config.toml"; \
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
        podman|docker|apple|bwrap|systemd|none) \
          printf '[sandbox]\nbackend = "%s"\n' "$backend" > "$overlay"; \
          echo "set $overlay -> [sandbox] backend = \"$backend\" (applies to all worktrees under $root)"; \
          ;; \
        *) echo "unknown backend '$backend' — expected: sprites | podman | docker | apple | bwrap | systemd | none" >&2; exit 1;; \
      esac; \
    fi

# Dogfood the local merge queue (the fold-actor): same isolated state root as
# `start`, but with `[merge_queue]` switched on via --set overrides (no daily
# config edit needed). Folds eligible worktree branches into the target branch
# with a compile gate; Super+K → "Integrate" / "Merge queue", or it auto-drains
# ~5s after an agent finishes. Override the gate with `gate='just test'` etc.
start-mq name="dev" gate="cargo build --workspace": build
    state="$HOME/.thegn-{{name}}/state"; run="$HOME/.thegn-{{name}}/run"; pidfile="$run/thegn.pid"; mkdir -p "$state" "$run"; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      echo $$ > "$pidfile"; exec env \
      "THEGN_ALACRITTY_CONFIG=$PWD/config/alacritty.toml" \
      "XDG_STATE_HOME=$state" \
      "XDG_RUNTIME_DIR=$run" \
      "THEGN_NO_MIGRATE=1" \
      {{bin}} --set merge_queue.enabled=true \
              --set 'merge_queue.gate_command={{gate}}' \
              --set merge_queue.regenerate_command="cargo update --workspace"

# Build and open the native host in a fresh ghostty window with the FULL
# dev/debug/profiling toolchain wired up. ghostty runs a hermetic, perf-tuned
# profile (config/ghostty.config: --config-default-files=false keeps your
# personal ghostty config out): no decorations/scrollback/URL-detection, vsync
# off for minimum input-to-present latency, and a dedicated single-instance
# process so the pidfile + `pgrep -n thegn` + SIGUSR2 drill all hit it. Plus:
#   - binary built with the `profiling` feature → SIGUSR2 flamegraph capture
#     (kill -USR2 once to start sampling, again to dump);
#   - every instrumentation channel on: startup waterfall + frame + hydrate +
#     perf logs land in $XDG_STATE_HOME/thegn/logs/thegn.log, and the
#     runtime self-profiler rollup is enabled (THEGN_PERF=1);
#   - state stays isolated per instance (~/.thegn-<name>).
# NOTE: this is a DEBUG binary (~2.5x slower than release), so read the
# flamegraph/perf rollup for structure & relative cost — for absolute timings
# use the release-grade `just bench` / `just bench-idle` harnesses.
# Optional `backend=` flips THIS repo's worktrees onto a sandbox backend /
# managed provider (e.g. `just start-term dev backend=sprites`) — see
# `_apply-backend`. Empty (the default) leaves config untouched.
start-term name="dev" backend="": build-profiling (_apply-backend backend)
    state="$HOME/.thegn-{{name}}/state"; run="$HOME/.thegn-{{name}}/run"; pidfile="$run/thegn.pid"; mkdir -p "$state" "$run"; \
      if [ -f "$PWD/.envrc.local" ]; then set -a; . "$PWD/.envrc.local"; set +a; fi; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      echo "profiler: 'kill -USR2 \$(pgrep -n thegn)' to start sampling, again to dump → $state/thegn/profiles/"; \
      echo "logs: $state/thegn/logs/thegn.log (startup waterfall + frame/hydrate/perf)"; \
      echo "bridge: $([ -x "$PWD/target/debug/thegn-musl" ] && echo "present (reverse tunnel :8484 live)" || echo "MISSING — run 'just bridge'; the nix-cache :8484 tunnel is disabled")"; \
      {{ _detach }} \
      command -v ghostty >/dev/null 2>&1 || { echo "start-term needs 'ghostty' on PATH (this recipe launches a dedicated window; use 'just start' for the current terminal)" >&2; exit 1; }; \
      _detach ghostty --config-default-files=false --config-file="$PWD/config/ghostty.config" -e sh -lc \
      'pidfile="$1"; shift; echo $$ > "$pidfile"; exec env "$@"' \
      sh "$pidfile" \
      "XDG_STATE_HOME=$state" \
      "SPRITES_TOKEN=${SPRITES_TOKEN:-}" \
      "THEGN_LOG=info,thegn::frame=debug,thegn::hydrate=debug,thegn::perf=debug" \
      "THEGN_PERF=1" \
      "$PWD/{{bin}}"

# Same dev/profiling/instrumentation rig as `start-term`, but a RELEASE binary —
# the daily-driver launcher. `start-term` stays debug for fast `cargo watch`
# rebuilds (`just dev-tui`); this gets the ~2.5x release speedup while keeping
# every log channel + the SIGUSR2 flamegraph profiler on, so live perf readings
# (frame render_ms, the thegn::perf rollup, idle CPU) reflect real shipped cost
# instead of the debug penalty. Use this to inhabit thegn all day.
# LOGGING IS MAXED OUT here for crash diagnosis: THEGN_LOG=debug globally with
# all thegn crates at trace → $logs/thegn.log, RUST_BACKTRACE=full, and the
# host's stderr (where panics print, normally swallowed when the ghostty window
# closes on a crash) is captured to $logs/stderr.log. After a crash, read
# stderr.log first (the panic + backtrace), then thegn.log for the lead-up.
# Optional `backend=` flips THIS repo's worktrees onto a sandbox backend /
# managed provider before launch — `just start-term-release backend=sprites`
# dogfoods the thegn repo onto sprites (auto-scaffolds the global
# `[env.sprites]`; needs the `sprite` CLI + SPRITES_TOKEN). Also accepts a real
# sandbox backend: podman|docker|bwrap|systemd|none. Affects ONLY this repo (a
# `.thegn.toml` overlay at the main worktree); empty leaves config untouched.
# See `_apply-backend` for the full mechanics.
start-term-release name="dev" backend="": release-profiling (_apply-backend backend)
    state="$HOME/.thegn-{{name}}/state"; run="$HOME/.thegn-{{name}}/run"; pidfile="$run/thegn.pid"; logs="$state/thegn/logs"; mkdir -p "$state" "$run" "$logs"; \
      if [ -f "$PWD/.envrc.local" ]; then set -a; . "$PWD/.envrc.local"; set +a; fi; \
      if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi; \
      : "ROTATE the release daemon + detached pane shells before relaunch — otherwise the persisted daemon reattaches OLD-binary pane sessions and a rebuild is never reflected (the 'restarted but still bwrap/sh-5.3' trap)."; \
      pkill -f "release/thegn[ ]daemon" 2>/dev/null || true; \
      pkill -f "dtach[ ]-A[ ][^ ]*/tg-socket-" 2>/dev/null || true; \
      sleep 0.3; \
      echo "profiler: 'kill -USR2 \$(pgrep -n thegn)' to start sampling, again to dump → $state/thegn/profiles/"; \
      echo "logs: $logs/thegn.log (full trace: startup/frame/hydrate/perf + every crate) + $logs/stderr.log (panic message + full backtrace)"; \
      echo "sprites token: $([ -n "${SPRITES_TOKEN:-}" ] && echo "loaded (len ${#SPRITES_TOKEN})" || echo "NOT set — sprites envs will halt; put SPRITES_TOKEN in .envrc.local")"; \
      echo "bridge: $([ -x "$PWD/target/release/thegn-musl" ] && echo "present (reverse tunnel :8484 live)" || echo "MISSING — run 'just bridge'; the nix-cache :8484 tunnel is disabled")"; \
      {{ _detach }} \
      command -v ghostty >/dev/null 2>&1 || { echo "start-term-release needs 'ghostty' on PATH (this recipe launches a dedicated window; use 'just start' for the current terminal)" >&2; exit 1; }; \
      _detach ghostty --config-default-files=false --config-file="$PWD/config/ghostty.config" -e sh -lc \
      'pidfile="$1"; errlog="$2"; shift 2; echo $$ > "$pidfile"; exec env "$@" 2>"$errlog"' \
      sh "$pidfile" "$logs/stderr.log" \
      "XDG_STATE_HOME=$state" \
      "SPRITES_TOKEN=${SPRITES_TOKEN:-}" \
      "RUST_BACKTRACE=full" \
      "THEGN_LOG=debug,thegn=trace,thegn_core=trace,thegn_svc=trace" \
      "THEGN_PERF=1" \
      "$PWD/target/release/thegn"

# Your REAL instance — real DB, real worktrees, real config — with the
# profiler + perf rollup + a DISK-CAPPED trace log. `start-term-release` is the
# same instrumentation against a THROWAWAY state root (~/.thegn-<name>); this is
# the one to use when the thing you want to diagnose only happens in your actual
# session (your worktrees, your panes, your queue).
#
# THE CAP IS THE POINT. Verbose logging goes to the rotating file sink, which is
# hard-bounded at `size_mb x (max_files + 1)` — 120 MB by default here. Raise the
# verbosity and the cap SHRINKS your history, it never grows the footprint:
#   just live level=trace        # firehose, same 120 MB ceiling
#   just live size_mb=5 files=2  # 15 MB ceiling, for a nearly-full disk
# The host role writes NOTHING to stderr (only `Role::Cli` does), so `stderr.log`
# collects panics and the odd early-startup line — kilobytes, not a firehose.
# It is truncated on each launch rather than appended, so it cannot creep either.
#
# For the compositor `THEGN_LOG` being set IS the request for logs, so the file
# sink is forced on regardless of `[log] file` (a `Role::Host` has no stderr
# layer — honouring `file = false` would mean asking for logs and getting none).
# What the host did NOT honour until now was the rotation/dir knobs: it passed
# hardcoded defaults, pinning the ceiling at 5 MB x 5 whatever you configured.
#
# THIS REPLACES YOUR RUNNING INSTANCE. Two hosts cannot share one state root
# (one SQLite cache, one daemon socket, one set of pane sessions), so the
# existing host + daemon + detached pane shells are rotated first — same reason
# `start-term-release` does it. Panes come back on reattach; unsaved work in a
# pane does not.
live level="debug" size_mb="20" files="5":
    #!/usr/bin/env bash
    set -euo pipefail
    just release-profiling
    state="${XDG_STATE_HOME:-$HOME/.local/state}"; logs="$state/thegn/logs"
    mkdir -p "$logs"
    echo "state:  $state/thegn (YOUR REAL db + session)" >&2
    echo "logs:   $logs/thegn.log — capped at $(( {{size_mb}} * ({{files}} + 1) )) MB total ({{size_mb}} MB x {{files}} rotations + active)" >&2
    echo "stderr: $logs/thegn-stderr.log (panics + backtrace; truncated per launch)" >&2
    echo "perf:   THEGN_PERF=1 rollup; 'kill -USR2 \$(pgrep -n thegn)' to start sampling, again to dump → $state/thegn/profiles/" >&2
    # Rotate the old instance: a stale daemon otherwise reattaches pane sessions
    # from the PREVIOUS binary, so a rebuild silently never takes effect.
    pkill -f "release/thegn[ ]daemon" 2>/dev/null || true
    pkill -f "[t]hegn daemon --socket" 2>/dev/null || true
    sleep 0.3
    exec env \
      "RUST_BACKTRACE=full" \
      "THEGN_LOG={{level}},thegn={{level}},thegn_core={{level}},thegn_svc={{level}}" \
      "THEGN_LOG_ROTATION_SIZE_MB={{size_mb}}" \
      "THEGN_LOG_MAX_FILES={{files}}" \
      "THEGN_PERF=1" \
      target/release/thegn 2>"$logs/thegn-stderr.log"

# Install/update the native thegn host onto your PATH (standalone, non-Nix):
# builds release artifacts, installs `tg` as the dedicated alacritty launcher,
# `tg-tui` for the current terminal window, and the direct `thegn`
# native-host binary. Pass a bindir to override the default (~/.local/bin),
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

# --- release artifacts -------------------------------------------------------

# Build the release archive + checksum for THIS machine's target, byte-for-byte
# the way `.github/workflows/release.yml` does (taiki-e/upload-rust-binary-action):
# `cargo build --release --locked -p thegn-host --bin thegn --target <t>`, then a
# tar.gz with the binary at the ROOT (no leading directory — the Homebrew formula
# does `bin.install "thegn"`), plus `<archive>.sha256` holding `shasum -a 256`
# output. The checksum filename deliberately has no `.tar.gz` infix, matching the
# action and what RELEASING.md tells users to verify.
#
# Run this before tagging: with remote CI paused, it is the only way to find out
# that a release build is broken BEFORE the tag is public. Output lands in
# target/release-artifacts/.
#   just release-artifacts v0.1.0-alpha.3
release-artifacts tag:
    #!/usr/bin/env bash
    set -euo pipefail
    target="$(rustc -vV | sed -n 's|host: ||p')"
    archive="thegn-{{tag}}-$target"
    out="$PWD/target/release-artifacts"
    echo "building ${archive}…"
    cargo build --release --locked -p thegn-host --bin thegn --target "$target"
    rm -rf "$out/stage"; mkdir -p "$out/stage"
    cp "target/$target/release/thegn" "$out/stage/thegn"
    # Same `include:` set as the release workflow — a dual-licensed artifact
    # must carry its license text, and the rehearsal must match what ships.
    cp LICENSE-MIT LICENSE-APACHE README.md "$out/stage/"
    (cd "$out/stage" && tar czf "../$archive.tar.gz" thegn LICENSE-MIT LICENSE-APACHE README.md)
    # `shasum -a 256`, not `sha256sum`: macOS has no GNU coreutils by default,
    # which is the same fallback the release action makes.
    (cd "$out" && shasum -a 256 "$archive.tar.gz" >"$archive.sha256")
    rm -rf "$out/stage"
    (cd "$out" && shasum -a 256 -c "$archive.sha256")
    echo "  $out/$archive.tar.gz"
    echo "  $out/$archive.sha256"
    echo "sha256: $(cut -d' ' -f1 <"$out/$archive.sha256")   # paste into packaging/homebrew/thegn.rb"

# Verify a built release archive end to end: the layout the Homebrew formula
# assumes, that the binary runs, and (on macOS) that it carries no quarantine
# attribute. Catches a broken archive before a tag rather than after.
release-verify tag:
    #!/usr/bin/env bash
    set -euo pipefail
    target="$(rustc -vV | sed -n 's|host: ||p')"
    out="$PWD/target/release-artifacts"; archive="thegn-{{tag}}-$target"
    (cd "$out" && shasum -a 256 -c "$archive.sha256")
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
    tar xzf "$out/$archive.tar.gz" -C "$tmp"
    [ -x "$tmp/thegn" ] || { echo "archive has no root-level 'thegn' — the Homebrew formula's bin.install would fail" >&2; exit 1; }
    for f in LICENSE-MIT LICENSE-APACHE; do
      [ -f "$tmp/$f" ] || { echo "archive is missing $f — thegn is dual-licensed and the text must ship with the binary" >&2; exit 1; }
    done
    got="$("$tmp/thegn" --version)"
    echo "runs: $got"
    case "$(uname -s)" in
    Darwin)
      if xattr -l "$tmp/thegn" 2>/dev/null | grep -q com.apple.quarantine; then
        echo "unexpected: the freshly built binary is quarantined" >&2; exit 1
      fi
      echo "no com.apple.quarantine on the built binary (as expected — quarantine is applied by the DOWNLOADER, not the build)"
      ;;
    esac
    echo "release-verify: ok"

# --- macOS launcher ----------------------------------------------------------

# Generate/refresh the macOS `thegn.app` launcher in ~/Applications, pointed at
# whichever thegn is on PATH (nix profile, ~/.local/bin, Homebrew). Double-clicking
# it — or hitting it from Spotlight/Raycast/Alfred/the Dock — opens a terminal
# emulator running thegn; it is the Darwin counterpart to the `.desktop` entry
# install.sh writes on Linux. `./install.sh` already does this for source installs,
# so this recipe is for the Nix/Homebrew paths, which never run install.sh.
# Both arguments are positional (just passes `k=v` through verbatim, so write
# the values alone):  just macos-app "$(command -v thegn)" /Applications
macos-app bin="" dest="":
    #!/usr/bin/env bash
    set -euo pipefail
    args=(--alacritty-config "$PWD/config/alacritty.toml")
    # `[ … ] && …` as the whole statement would exit under `set -e` when the
    # default (empty) argument is used — keep the `if` form.
    if [ -n "{{bin}}" ]; then args+=(--bin "{{bin}}"); fi
    if [ -n "{{dest}}" ]; then args+=(--dest "{{dest}}"); fi
    ./packaging/macos/make-app.sh "${args[@]}"

# Re-render the owl app icons from the sprite in crates/thegn-host/src/owl.rs:
# config/thegn.svg (Linux launcher entry) + packaging/macos/thegn.icns (the .app
# bundle). Both are committed; run this after touching SPRITE/PALETTE in owl.rs.
icons:
    python3 scripts/gen-owl-icon.py
    python3 scripts/gen-owl-icns.py

# --- fonts ------------------------------------------------------------------

# Installed Nerd Font families (candidates for `just font`).
# fontconfig is Linux/BSD; stock macOS has none, so fall back to listing the
# font directories the way the in-app picker does (see `font.rs`).
fonts:
    #!/usr/bin/env bash
    set -uo pipefail
    if command -v fc-list >/dev/null 2>&1; then
      fc-list : family | tr ',' '\n' | grep -i 'nerd font' | grep -iv 'mono\b.*propo\|propo' | sort -u
    else
      echo "fc-list not found (no fontconfig) — listing font files instead:" >&2
      ls "$HOME/Library/Fonts" /Library/Fonts /System/Library/Fonts 2>/dev/null \
        | grep -iE '\.(ttf|otf|ttc)$' | sed -E 's/-(Thin|ExtraLight|Light|Regular|Medium|SemiBold|Bold|ExtraBold|Black|Italic|BoldItalic)?\.[^.]+$//' \
        | grep -i 'nerd' | sort -u
    fi

# Switch the bundled alacritty profile's font live (alacritty live-reloads,
# so the change is instant in a running session). e.g.
#   just font name="JetBrainsMono Nerd Font"
# `sed -i` is not portable: GNU takes a bare -i, BSD/macOS requires a backup
# suffix (`-i ''`). Write to a temp file and move it into place instead — one
# form that works everywhere.
font name:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
    sed 's/^normal = { family = ".*" }$/normal = { family = "{{name}}" }/' config/alacritty.toml > "$tmp"
    cat "$tmp" > config/alacritty.toml
    echo "font → {{name}} (alacritty live-reloads in place)"

# --- sandbox base image (hosts-as-resources) ---------------------------------

# Build the base sandbox image for THIS machine's arch and load it into podman
# (`nix/sandbox-image.nix` → streamLayeredImage). The provisioner then delivers
# it registry-lessly to hosts (`thegn host provision <name>`).
image-build:
    nix build .#sandbox-image
    ./result | podman load

# Publish both arches + a manifest list to a registry, then print the list
# digest to pin as DEFAULT_BASE_DIGEST (thegn-core/src/image.rs). Needs
# native builders per arch (or remote builders); run on CI normally.
#   just image-publish registry=ghcr.io/you tag=v1
image-publish registry tag="v1":
    #!/usr/bin/env bash
    set -euo pipefail
    ref="{{registry}}/thegn-sandbox:{{tag}}"
    # `uname -m` is amd64/arm64 on some systems and x86_64/aarch64 on others;
    # Apple silicon in particular reports arm64.
    arch="$(uname -m)"; case "$arch" in x86_64|amd64) oci=amd64;; aarch64|arm64) oci=arm64;; *) echo "unsupported arch $arch" >&2; exit 1;; esac
    nix build .#sandbox-image
    ./result | podman load
    podman tag thegn-sandbox:latest "$ref-$oci"
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
    ref="{{registry}}/thegn-fly-sandbox:{{tag}}"
    nix build .#fly-sandbox-image
    ./result | podman load
    podman tag thegn-fly-sandbox:latest "$ref"
    podman push "$ref"
    echo "pushed $ref — point the Fly env at it:"
    echo "  [env.fly.provider]"
    echo "  template = \"image:$ref\"   # boots from the baked image (fast path)"
