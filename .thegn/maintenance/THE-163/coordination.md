# Primary coordination brief — THE-163

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

A plugin status update sets `dirty` **and** `bars_dirty`, forcing a full
recompose even when only one bounded bar segment changed.

## This lane touches the render path — the invariants are the hard part

From CLAUDE.md, and these are enforced by tests in `just test`:

- **The render decision is pure.** `render_plan::plan` -> `Skip` / `Panes` /
  `Full`. Pane output must never recompose chrome. Do not make `plan` consult
  plugin state; give it the damage it needs as input.
- **0% idle.** A repeated identical status update must produce **no frame at
  all** — that is an explicit acceptance criterion here, and the idle-poll
  ratchet will catch a regression.
- Exhaustive unit tests in `render_plan` lock the work-shape. Extend them; do
  not weaken them.

## Primary decisions

- **Narrow the damage, do not add a second damage mechanism.** Use whatever
  region/damage representation already exists. If there is genuinely none for
  bars, say so and propose the smallest addition rather than inventing a
  parallel system.
- **Structural changes still invalidate broadly**: add/remove a contribution,
  a size/placement change, a theme change. Only _content-only_ updates narrow.
  Getting this backwards is a visual corruption bug, so test both directions.
- **De-duplicate identical updates at the source**, so an unchanged status never
  reaches the render decision at all.
- THE-165 and THE-161 are listed as blockers and are **unlanded**. Narrowing
  render damage does not depend on generation-tagging or output bounding — do
  not implement either. If you find it genuinely does, STOP and report.

## Tests

Assert damaged rows/bytes for a status-only update versus a structural one, and
assert a repeated identical update yields `Skip`.

## Scope

`crates/thegn-host/src/run.rs` (the dirty-flag site only — **do not grow
run.rs**, it is a named god-file; put new logic in a sibling module),
`crates/thegn-host/src/render_plan.rs`, and their tests.
