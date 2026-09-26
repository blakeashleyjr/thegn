# Primary review + greenlight — THE-195

Reviewing row 597's investigation. **APPROVED to implement.**

Confirmed: `api.rs` carries a **duplicate CLI registry** reporting 55/92, while
the session registry / core alias-aware ledger reports 88/92 with four
documented gaps. Two hard-coded tables, so the diagnostic is untrustworthy.

## Your bonus finding is accepted and in scope

> `doctor` reuses the wrong ledger

Good catch — fix it in the same change. A second consumer reading the wrong
authority is the same defect, and leaving it would mean `doctor` and
`api coverage` still disagree after this lands. That would be an odd place to
stop.

## Restated decisions

- **One authority, derived.** Both reports read the same registry. CLAUDE.md is
  explicit that the control API, gRPC, CLI verbs, MCP tools and plugin host
  calls all project `thegn_core::capability::CATALOG` — so the alias-aware
  ledger is the authority and `api.rs`'s private table is the thing to delete.
  Fix the direction of the dependency; do not sync two lists.
- **Exclusions are data with reasons**, machine-readable, one category each.
  Covered, or excluded-with-a-reason. No third state, no silent gap.
- **A new/removed command or route must fail a drift test** until the one
  authority is updated. That is what stops this recurring.
- **Do not pin the counts as literals.** Lock them through the generated data or
  the test becomes the third source of truth — which is how 55 and 88 came to
  coexist.
- Include revision/schema metadata in both human and JSON output, as the issue
  asks.

## Out of scope

Closing the gaps themselves — THE-201/202/203/204. This lane makes the number
**true**, not 92/92. If the honest count turns out to be neither 55 nor 88, that
is a finding: report it, and do not adjust anything to hit a target.

## Validation

`cargo check -p thegn-host --all-targets` plus the focused api-coverage and
drift tests. Attempt them; **if `nix develop` fails inside your sandbox** (the
pipeline env mounts `/nix/store` read-only) say so and stop — the primary runs
them. State which you actually ran.
