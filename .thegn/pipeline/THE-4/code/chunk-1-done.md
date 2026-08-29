# THE-4 chunk 1 completion

Implemented the dev-loop documentation alignment and closed the
`document-dev-loop-policy` OpenSpec record.

## Changed

- Updated contributor, README, local-CI, coverage, muse, and in-app help
  guidance to use scoped iteration checks and defer expensive gates.
- Updated the bundled TUI and pipeline skills with the same policy, including
  crate-scoped host builds and persistent `muse session` verification.
- Corrected the OpenSpec proposal, added its design artifact, completed its
  tasks, synced the accurate heavy-gate guard requirement into
  `openspec/specs/architecture-gates/spec.md`, and archived the complete
  change at `openspec/changes/archive/2026-08-29-document-dev-loop-policy/`.
- Left the guard, hook wiring, justfile, flake, Rust sources, and ratchets
  unchanged.

## Validation

- `just quick thegn-host` — passed.
- `cargo nextest run -p thegn-host help mq_assets` — passed: 83 tests,
  2,589 skipped.
- `openspec validate --all --strict` — passed: 171/171 items.
- `git diff --check` — passed.

## Unverified

- Full `just test`, `just ci`, coverage, and e2e were intentionally not run;
  they remain pre-push/CI/final-UI gates under the repository dev-loop policy.
