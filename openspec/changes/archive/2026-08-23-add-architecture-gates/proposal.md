## Why

The 2026-08-22 extensibility audit found that every architectural invariant CLAUDE.md states — `thegn-core` has no tokio/termwiz dependency, platform `#[cfg]` code lives in `platform/`, draw sites go through the `caps`/`wire.rs` chokepoints, the idle loop never polls, vendor CLIs (`gh`) are called only from the forge impl, ignored `Result`s are marked best-effort — is convention-only: nothing fails when it is broken. The repo already owns the right idioms (shrink-only allowlist ratchets, grep guards in `just lint`, schema-walk coverage tests); this change applies them to those invariants, seeded with today's debt so they land green and can only shrink. It also closes the gate gaps the audit listed: `just term-check` exists but is not in CI, no feature-matrix or MSRV build exists, and `just ci` cannot pass locally because it includes the known-broken e2e suite.

## What Changes

- **Reusable ratchet helpers**: `test/ratchet.sh <name> <grep -E pattern> <paths…>` (file-level, shrink-only, bidirectional stale check, `RATCHET_UPDATE=1` rewrites) for `just lint`; a Rust `file_ratchet(name, hit, why)` helper extracted from `caret_ratchet_tests.rs` (which is refactored onto it) and mirrored into `thegn-core` under `test-utils`.
- **Architecture gates** (all seeded with current debt):
  - crate boundaries via `crates/thegn-core/tests/crate_boundaries.rs` (tokio, termwiz, portable-pty, reqwest, octocrab, axum, alacritty_terminal may only be direct deps of their owner crates; thegn-core carries none) + `deny.toml` outright bans of `vt100`/`russh`;
  - `test/platform-cfg-ratchet.txt` — `#[cfg(unix|windows|target_os|target_family)]` outside `platform/` modules, `termcaps`, `sandbox*`;
  - `test/color-literal-ratchet.txt` + `test/glyph-literal-ratchet.txt` — color/glyph literals outside `wire.rs`/`caps.rs`/`theme*`/`glyphs`;
  - `test/forge-leak-ratchet.txt` — `thegn_core::github::` / `Command::new("gh")` outside the forge impl files;
  - idle-poll: `run.rs` poll-timeout decision extracted to a pure `idle_poll::poll_timeout` with tests, plus a lint grep that the only non-`None`/non-`ZERO` `poll_input(` site consumes it;
  - `[workspace.lints.clippy] let_underscore_future = "deny"` (`let_underscore_must_use` rejected: it flags the sanctioned `let _ = best_effort()` idiom) + `test/ignored-result-ratchet.txt`;
  - `test/async-fn-in-trait-ratchet.txt` — `async fn` in trait definitions (the seam rule), burned down as seams migrate.
- **CI/justfile**: `just check-features` (`--all-features` + each named feature), `just check-msrv` (1.89), `term-check` added to `ci` and as a CI job, `ci` split into `ci` (server gate, no e2e) and `ci-local: ci e2e`, `test/stale-docs-guard.sh` (`vt100|russh|no IPC|CI, every push`).
- **Stale-doc fixes**: CLAUDE.md (vt100, russh, "gh wrapper", e2e-in-ci), `emulator.rs:8`, `thegn-svc/src/lib.rs:4`, `README.md`, `docs/testing-with-muse.md`, `docs/cli.md`, the ~15 "file-size ratchet" doc-comments.
- **`docs/help` ⇔ `SOURCES` enumeration test** (cheap; rides along).

## Capabilities

### New Capabilities

- `platform-portability`: where platform-conditional code may live, crate dependency boundaries, the terminal-capability matrix in CI, the feature/MSRV matrix, and the ratchets that enforce each.
- `architecture-gates`: the ratchet mechanism itself (shrink-only allowlists, bidirectional stale checks, regeneration) and the invariant → gate mapping for the event loop, render chokepoints, forge isolation and ignored results.

### Modified Capabilities

- `event-loop`: the idle-never-polls contract gains an explicit pure decision function and a gate (delta adds a requirement; existing ones unchanged).

## Impact

- Files: `test/ratchet.sh`, `test/*-ratchet.txt` (6 new), `test/stale-docs-guard.sh`, `crates/thegn-host/src/{ratchet.rs,caret_ratchet_tests.rs,platform_ratchet_tests.rs,idle_poll.rs,run.rs}`, `crates/thegn-core/src/test_support/ratchet.rs` (+ twins in core/svc/media/metrics), `deny.toml`, `Cargo.toml` (workspace lints), `justfile` (`lint`, `ci`, `ci-local`, `check-features`, `check-msrv`), `.github/workflows/ci.yml`, `flake.nix` (MSRV toolchain or `cargo-msrv`), `docs/testing-with-muse.md`, CLAUDE.md, `openspec/config.yaml`.
- No runtime behaviour change except `idle_poll` (a pure extraction — render_plan invariants stay green).
- Roadmap: tasks.md **A.1** (core architecture), **AX** (terminal compat — term-check gated), Windows/macOS groups (cross-check stays; platform ratchet is the new gate). Design source: the 2026-08-22 audit plan §2.2 B0–B1 + B2 (ci bits).
