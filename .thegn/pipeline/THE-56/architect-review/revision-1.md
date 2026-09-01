# THE-56 revision 1 — reconcile the schema migration with current main

## Gap

This implementation was built from merge-base `caef2f0e` (schema v61), but
the current base branch `main` is now at schema v65. `main` has already
allocated:

- v62 for `session_forks`;
- v63 for `pr_review_cache` and the pipeline lease companion;
- v64 for trusted automation state/audit; and
- v65 for review-task metadata and forge-action retry state.

THE-56 currently changes `SCHEMA_VERSION` to 63 and defines
`verify_v63_schema` for `autopilot_runs`. Merging this branch into current
main would therefore collide with existing version ownership and either lose
the landed migrations or stamp the database at the wrong version. The
architect design itself says the version must be reconciled after the
in-flight migrations; that reconciliation has not happened on this branch.

## Required correction

Rebase or merge the current `main` into the THE-56 branch, preserving all
landed v62–v65 schema code and all unrelated main-side changes. Then add the
autopilot journal as the next genuinely available migration (currently v66;
re-check `main` immediately before editing), with:

- one additive/idempotent migration in the existing migration ladder;
- a schema verifier chained through the prior verifier (currently the v65
  verifier), so prior report/notes, fork, review-cache, automation, and
  review-task contracts remain checked before the version stamp;
- the `autopilot_runs` table and its unique provider/account/issue claim
  constraint plus indexes retained unchanged in meaning; and
- migration tests for fresh creation, upgrade from the reconciled prior
  version, duplicate claim, and reopen/readback without dropping existing
  rows.

Do not reuse v63, replace the current `main` migration ladder, or create a
second verifier for a version already owned by another feature. Re-run the
core migration/config filters and the host autopilot filters after the
reconciliation.

## Scope

- `crates/thegn-core/src/db.rs`
- `crates/thegn-core/src/db_migrate.rs`
- `crates/thegn-core/src/db_autopilot.rs` (only if the reconciled schema/API
  requires an adjustment)
- relevant THE-56 migration tests and handoff documentation

The reviewer's small invariant fixes are already committed in
`a1027ea5`; preserve them while reconciling the migration.
