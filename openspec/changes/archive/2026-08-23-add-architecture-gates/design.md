## Context

Phase 1 of the extensibility convergence. Phase 0 (`add-seam-foundation-and-capability-catalog`) gave the codebase the seam pattern and the capability catalog; this phase makes the architecture's standing invariants fail loudly when broken, before the larger refactors (forge unification, git backend selection, editor seam) start moving code around. Every gate reuses an idiom already in the repo: `caret_ratchet_tests.rs` (shrink-only allowlist with a bidirectional stale check), the `Command::new("git")` grep guard at `justfile:477`, `test/brand-guard.sh`, and `cli_help::GROUPS`.

## Goals / Non-Goals

**Goals:** every CLAUDE.md invariant has a named gate; gates land green by seeding today's debt; allowlists only shrink; `just ci` is green-able locally; term-check, feature matrix and MSRV are CI jobs.

**Non-Goals:** burning down the seeded debt (later phases do that as a side effect — e.g. forge unification empties `forge-leak-ratchet.txt`); surface-consistency gates that need data from other refactors (`Action`⇄`ActionSpec` round-trip, env-overlay completeness, hm-module drift, `--json` snapshots, unknown-key pass) — those are the following change; making macOS/Windows CI blocking.

## Decisions

- **Two helpers, one rule.** Shell (`test/ratchet.sh`) when the predicate is a pure regex across crates and should fail in `just lint` with zero compile; Rust (`file_ratchet`) when the predicate needs Rust data or the crate's own suite is where the failure belongs. Both: comment-stripped scan, allowlist ⊆ hits **and** hits ⊆ allowlist, header comment states the reason and burn-down target, `RATCHET_UPDATE=1` / `THEGN_RATCHET_UPDATE=1` regenerates (the `just help-ratchet-update` idiom).
- **Shell ratchets scan with `git grep --untracked`** — plain `git grep` only sees tracked files, so a brand-new offending file would pass until staged (caught by the gate-firing probe).
- **File-level, not line-level.** Line-level allowlists churn on every unrelated edit and the ignored-result case alone has ~1600 sites; a file is the unit that moves when debt is paid.
- **Crate boundaries via a test over the workspace manifests** (`crates/thegn-core/tests/crate_boundaries.rs`), not cargo-deny `wrappers`: tried first, `wrappers` demands _every_ direct parent of a banned crate be listed — including the dozens of third-party crates that depend on tokio — so it can't express "only these workspace crates". `deny.toml` keeps the outright bans (`vt100`, `russh`). `thegn-core → thegn-media`/`sysinfo` is pinned as the sanctioned leaf edge.
- **Idle poll as a pure function.** `render_plan` locks the render decision; `idle_poll::poll_timeout(defer, dirty, pending_input, budget_exhausted) -> Option<Duration>` locks the poll decision the same way, and the lint grep ensures the loop has exactly one consumer of it. Render/event-loop impact: none (pure extraction; the damage channels are untouched).
- **`let_underscore_future` as deny** — a dropped future never ran. `let_underscore_must_use` was tried and rejected: `Result` is `#[must_use]`, so it flags the sanctioned `let _ = best_effort()` idiom at every site. The `let _ =` discipline is the file-level ratchet.
- **`ci` vs `ci-local`.** The server-side gate must be green-able; e2e stays in `ci-local` (and opt-in in CI) until its timeout is fixed and baselines re-recorded — the existing CLAUDE.md note, made true of the recipe.
- **MSRV check via a second toolchain in the dev shell** (`rust-bin.stable."1.89.0"` as `cargo-msrv` alternative is heavier); `just check-msrv` = `cargo +1.89.0 check --workspace --locked`. If the flake can't carry two toolchains cheaply, fall back to `cargo msrv verify` — decided during implementation, documented in the justfile recipe.
- **Stale-docs guard is a token list**, same shape as `test/brand-guard.sh`, with a file allowlist for intentional historical mentions (CHANGELOG, archived specs).
- Help context: no new interactive surface; `docs/help/terminal-compatibility.md` gains a sentence that `thegn doctor`'s matrix is CI-gated.

## Risks / Trade-offs

- [Seeded allowlists are large (platform-cfg ~60 files, ignored-result ~100)] → the header comment names the burn-down target per file class; size is visible, not hidden, and it only shrinks.
- [`cargo-deny` wrapper lists drift when a crate legitimately gains a dep] → the failure message names the crate and the ban; adding a wrapper is a one-line, reviewed change.
- [`let_underscore_future = deny` may surface real bugs] → fix, don't allowlist; the phase budget includes it.
- [MSRV toolchain adds to `nix develop` closure] → evaluate size; fall back to `cargo-msrv`.
- [Removing e2e from `ci` weakens the documented pre-PR gate] → `ci-local` keeps it for anyone who wants it; CI behaviour is unchanged (it was already opt-in).

## Migration Plan

Additive. Lands as one PR; each gate is its own commit so a false positive can be reverted alone. Rollback = revert.

## Open Questions

- MSRV mechanism (second toolchain vs `cargo-msrv`) — resolved during 5.2.
