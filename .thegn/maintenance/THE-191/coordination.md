# Primary coordination brief — THE-191

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

`plugin list` reports configuration/install state but not runtime health, so an
operator cannot tell a healthy plugin from one the supervisor disabled after a
crash loop.

## Primary decisions

- **One authoritative supervisor snapshot.** Do not infer health from config
  files, process existence, or a second cache. If the snapshot is unavailable,
  say so explicitly — do not fall back to guessing.
- **Stable state names.** `disabled-by-config`, `starting`, `healthy`,
  `degraded`, `crash-disabled`, `stopped`, `stale-generation`. These are a
  public contract once printed: define them as a typed enum with one
  serialization used by both human and JSON output, not two format strings.
- **Bound and redact the last-failure text.** A plugin's stderr is untrusted and
  may carry paths or tokens. Bound it and strip control characters — reuse the
  existing bounded-display machinery rather than writing a third one.
- Include restart/backoff metadata, bounded.
- THE-165 (generation tagging) is listed as a blocker and is **unlanded**. Report
  whatever generation/health the supervisor tracks **today**; if there is no
  generation concept yet, report the other states and record `stale-generation`
  as a follow-up rather than inventing a generation mechanism here.

## Tests

Drive crash -> backoff -> disable and assert the transitions; assert human and
JSON agree on the state name; assert the failure text is bounded and
control-free.

## Scope

`crates/thegn-host/src/cmd/plugin.rs`, the supervisor snapshot type, and tests.
If this changes the control surface, `docs/api/control-v1.json` may move —
flag it in your report; the primary regenerates it centrally.
