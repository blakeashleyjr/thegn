# Primary coordination brief — THE-195

## PRIMARY AUTHORIZATION — you MAY run `cargo check` and focused `cargo test`

The stage prompt says not to run Cargo because the primary centrally schedules
Rust validation. **The primary grants a narrow exception**, because the previous
batch repeatedly returned `implementation-ready` for code that did not compile,
and each round-trip costs far more than the checks would.

Authorized, as often as you need:

```
nix develop --command cargo check -p <crate> --all-targets
nix develop --command cargo test -p <crate> --lib <narrow-filter>
```

`--all-targets` is required — the library often builds when the **test** targets
do not. Keep the test filter narrow; it must not become a workspace run.

Still forbidden, and still the primary's job: `cargo build`, unfiltered
`cargo test`, `nextest --workspace`, clippy, `just lint`, `just test`, `just ci`,
and anything full-workspace.

**Your row is not finished until the check is clean and the tests you added or
touched pass.** If you cannot get there inside your approved scope, report the
remaining failures verbatim as a blocker — that is a good outcome. Reporting
`implementation-ready` for code that does not build is not. `rustfmt` passing is
evidence of formatting only.

## Line numbers in the issue are STALE

Every citation comes from audit commit `299fc13`, not current `main`. The
previous batch found three issues whose headline defect was already fixed and
two whose file inventory was wrong. **Re-verify each citation on this branch and
report already-met criteria as met, with evidence, rather than re-fixing them.**

---

## The defect

`thegn api coverage` reports CLI coverage as 55/92 while the test ledger says
88/92. Two hard-coded tables, so the diagnostic is untrustworthy and drifts.

## Primary decisions

- **One authority, derived — not two literals reconciled.** Both the CLI report
  and the tests must read the same registry. CLAUDE.md is explicit that the
  control API, gRPC, CLI verbs, MCP tools and plugin host calls all project
  `thegn_core::capability::CATALOG`; that is the authority. If the CLI report is
  not reading it, that is the bug — fix the direction of the dependency rather
  than syncing two lists.
- **Exclusions are data with reasons**, machine-readable, one category each. An
  operation is covered, or excluded with a stated reason. There is no third
  state and no silent gap.
- **Adding or removing a command/route must fail a drift test** until the one
  authority is updated. That is what stops this recurring.
- **Do not pin the current counts as literals** in a test. Lock them through the
  generated data, or the test becomes the third source of truth.
- THE-190 (HTTP-only DTOs in the schema snapshot) is listed as a blocker and
  **is already landed on main** — you are unblocked.

## Out of scope

Closing the gaps themselves (THE-201/202/203/204). This lane makes the number
_true_; it does not make it 92/92. If fixing the reporting reveals that the real
count differs from both 55 and 88, that is a finding — report it, do not adjust
anything to hit a target.

## Scope

`crates/thegn-host/src/cmd/api.rs`, `crates/thegn-host/src/cmd/session.rs`, the
capability catalog if it needs a coverage projection, and the drift tests.
