## 1. Ratchet helpers

- [ ] 1.1 `test/ratchet.sh <name> <pattern> <paths…>`: comment-stripped `git grep -IlE`, allowlist ⊆ hits and hits ⊆ allowlist, header comment preserved, `RATCHET_UPDATE=1` rewrites; shellcheck-clean
- [ ] 1.2 `crates/thegn-host/src/ratchet.rs` (`#[cfg(test)]`): `allowlist`, `sources(exclude_prefixes)`, `code_only`, `file_ratchet(name, hit, why)`; refactor `caret_ratchet_tests.rs` onto it (both tests keep passing)
- [ ] 1.3 Mirror the helper into `crates/thegn-core/src/test_support/ratchet.rs` under `test-utils` so svc/media/metrics tests can use it

## 2. Crate boundaries

- [ ] 2.1 Verify wrapper lists with `cargo tree -e normal -i <crate> --depth 1` for tokio, termwiz, portable-pty, reqwest, octocrab, axum
- [ ] 2.2 `deny.toml`: `[[bans.deny]]` entries with `wrappers`; ban `vt100`, `russh`; `just deps-audit` green
- [ ] 2.3 Document the allowed `thegn-core → thegn-media/sysinfo` leaf edge in the deny.toml comment

## 3. Ratchets (seeded)

- [ ] 3.1 Platform-cfg: `platform_ratchet_tests.rs` in host (+ core/svc/media/metrics twins), `test/platform-cfg-ratchet.txt` seeded, excluding `platform/**`, `termcaps.rs`, `sandbox*.rs`
- [ ] 3.2 Color/glyph literal ratchets in host: `test/color-literal-ratchet.txt`, `test/glyph-literal-ratchet.txt` (pin `apps/bridge.rs` as a chokepoint with that reason)
- [ ] 3.3 Forge leak: `just lint` line `test/ratchet.sh forge-leak 'thegn_core::github::|use thegn_core::github|Command::new\("gh"\)' crates/…`; `test/forge-leak-ratchet.txt` seeded with the impl files marked as such
- [ ] 3.4 `async fn` in trait: `test/ratchet.sh async-trait '^\s*async fn .*;\s*$|#\[allow\(async_fn_in_trait\)\]'` over trait bodies; seed `test/async-fn-in-trait-ratchet.txt`
- [ ] 3.5 Ignored results: `[workspace.lints.clippy]` in `Cargo.toml` (`let_underscore_must_use = "warn"`, `let_underscore_future = "deny"`), every crate inherits via `[lints] workspace = true`; fix any real `let _ = future` hits; `test/ignored-result-ratchet.txt` file-level ratchet seeded

## 4. Idle poll

- [ ] 4.1 Extract `run.rs` poll-timeout arithmetic into `crates/thegn-host/src/idle_poll.rs` `poll_timeout(...) -> Option<Duration>`; loop calls it at the single `poll_input` site
- [ ] 4.2 Tests: `idle_never_polls`, `busy_batches_8ms`, deferred-work case; `render_plan` tests unchanged
- [ ] 4.3 `just lint` grep: every `poll_input(` in `crates/thegn-host/src` is `(None)`, `Duration::ZERO`, or the `idle_poll::` site

## 5. CI / justfile

- [ ] 5.1 `just check-features`: `cargo check --workspace --all-features` + per-feature checks; add to `ci` + ci.yml job
- [ ] 5.2 `just check-msrv` (second toolchain in `flake.nix` or `cargo-msrv`); add to `ci` + ci.yml job
- [ ] 5.3 Add `term-check` to `ci` and as a ci.yml job
- [ ] 5.4 Split `ci` (no e2e) / `ci-local: ci e2e`; update CLAUDE.md + `docs/testing-with-muse.md`
- [ ] 5.5 `test/stale-docs-guard.sh` (`vt100|russh|no IPC|CI, every push`, file allowlist for CHANGELOG/archives) wired into `just lint`

## 6. Stale docs + small gates

- [ ] 6.1 Fix: CLAUDE.md (vt100/russh/"gh wrapper"/e2e-in-ci), `crates/thegn-host/src/emulator.rs:8`, `crates/thegn-svc/src/lib.rs:4`, `README.md` russh, `docs/testing-with-muse.md:9`, `docs/cli.md` "no IPC", the "file-size ratchet" doc-comments (~15)
- [ ] 6.2 `help/pages.rs` test: `docs/help/*.md` set ⇔ `SOURCES` set (generated pages excluded)
- [ ] 6.3 `docs/help/terminal-compatibility.md`: note the matrix is CI-gated; `tasks.md` A.1 / AX entries cite this change

## 7. Gate

- [ ] 7.1 Run `just quick` per crate, the new ratchet tests, `just lint`, `just deps-audit`, `openspec validate --all --strict`; deliberately break one invariant of each kind to prove the gate fires, then revert (e2e skipped; `just ci` once it no longer includes e2e)
