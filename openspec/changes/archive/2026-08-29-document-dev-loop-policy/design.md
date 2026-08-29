# Design — document the dev-loop heavy-gate policy

## Approach

This is a prose and OpenSpec bookkeeping change. Update each human-facing
iteration recipe to lead with the cheap, scoped checks:

- `just quick <crate>` for lib/bin clippy;
- `cargo nextest run -p <crate> <filter>` for tests being touched; and
- a crate-scoped `cargo build -p thegn-host` only when a TUI executable needs
  refreshing.

Describe `just test`, `just lint`, `just coverage`, `just ci`, and full `just
e2e` as deliberate pre-push, diagnostic, pre-PR, or final UI gates. Keep a
persistent isolated `muse session` for interactive TUI verification, and make
`just e2e-update` an intentional baseline update followed by review.

## OpenSpec reconciliation

The active proposal and tasks are corrected to match the audit. The delta
documents only behavior present in `test/heavy-guard.sh`: its recognized heavy
invocations are refused with scoped alternatives; `THEGN_ALLOW_HEAVY=1` passes
through; missing dependencies or malformed input fail open; and mentions only
inside quoted text or heredoc bodies do not trigger the guard. The shell-runner
matcher is described only for the forms it actually recognizes. The corrected
delta is synced into the canonical `architecture-gates` specification, then
the complete active change is moved intact to the dated archive.

## Invariants and validation

No render decision, event-loop wake path, SQLite schema, runtime worker,
interactive surface, or help action is changed. Existing help and asset tests
are sufficient for the edited markdown. The implementation boundary is:

```sh
just quick thegn-host
cargo nextest run -p thegn-host help mq_assets
openspec validate --all --strict
```

Full workspace gates, coverage, and e2e remain final pre-push/CI/UI actions and
are not part of this iteration pass.
