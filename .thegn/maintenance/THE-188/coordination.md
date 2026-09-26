# Primary coordination brief — THE-188

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

Observe refreshes reuse a fixed historical end time, so a dashboard looks live
while re-querying the same stale window.

## Primary decisions

- **Relative and absolute windows are different types, not a flag.** A relative
  window ("last 15m") resolves against a `now` supplied per refresh; an absolute
  window is fixed data. Model them so an absolute range _cannot_ drift and a
  relative one _cannot_ be frozen — that makes both acceptance criteria
  structural rather than conditional.
- **One `now` per refresh cycle, captured once and passed down.** Every panel in
  a refresh shares it. Panels computing their own `now` is how a dashboard ends
  up internally inconsistent.
- **`now` must be injectable** so tests use a fake clock. No `SystemTime::now()`
  inside the query builder.
- Define pause/resume (a paused dashboard freezes its relative window; resume
  advances to current) and monotonic behaviour under clock skew — a backwards
  system clock must not produce an inverted range.
- THE-184 is listed as a blocker and is **unlanded**. Window advancement does
  not depend on concurrent panel fetching — do not implement THE-184's bounded
  queues/deadlines here.

## Tests

Fake clock: successive refreshes produce advancing, equal-duration ranges; an
absolute range is byte-identical across refreshes; all panels in one cycle share
start/end; a backwards clock does not invert the range.

## Scope

`crates/gtui-app/src/engine.rs` and its tests.
