# THE-591 closure readiness review — September 14, 2026

Reviewed landed main `1ef8228f` against the current full Linear description.
Independent maintenance/protocol lane verdict: ready for primary closure review.
No remaining THE-591-specific implementation or test gate was identified.

## Implementation

`db_aux.rs::persist_merge_outcome` validates typed context, takes an IMMEDIATE
transaction, compares the pre-fold registry and complete queue observations,
and performs one final-status upsert. It never writes a synthetic queued state.
Nullable result/conflict/error fields are deliberately replaced, while existing
nomination time and attempt count survive. Concurrent reassignment returns a
typed refusal; SQL errors roll back.

`integrate_persistence.rs` captures observations before fold work, validates the
complete projected report before writing, and runs lifecycle effects only after
the corresponding final state commits. Prepared/unprepared infrastructure holds
have no landed result. Committed Landed rows now call the integrated
`merge_lifecycle::apply_landed` with the actual result OID. Thus the old OpenSpec
task saying this integration is pending is historical, not a current code gap.

## Evidence inspected

- `/tmp/thegn-audit-combined-core-merge.log`: 119 tests passed, including all 14
  current `db_aux::merge_outcome_tests`. These include queued-rejecting SQL
  tripwires, exact nullable clearing, rollback, two-connection registry/queue
  reassignment, reader snapshots and simultaneous finalizers.
- `docs/audits/remediation-2026-09-13-artifacts/thegn-audit-combined-host-results.json`:
  all eight `integrate::persistence::tests` passed. They exercise final-only
  projection, commit-before-lifecycle triggers, SQL write failure with no
  lifecycle effects, stale/missing observations, reassignment and contradictory
  reports.
- `/tmp/thegn-native-unblocked-host-results.json`: the formerly blocked
  `failed_union_cannot_persist_a_land_or_run_landed_lifecycle` and
  `persist_defensively_refuses_legacy_speculative_landed_entries` both passed
  natively, as did the relevant actual cleanup fixtures. The earlier namespace
  failures are superseded by these exact test-name receipts.
- `docs/audits/local-main-landing-2026-09-14.md` records the reviewed local landing
  and passed native rerun, source/delivery/specification gates. No push or live
  cleanup is implied by fixture success.

Remaining THE-588/THE-596 cleanup contracts and the narrow THE-600/THE-593 test
follow-ups are separate issue obligations. They do not reinstate transient
requeue or missing committed-outcome wiring in THE-591. Its OpenSpec tasks and
Linear status should be reconciled with the landed evidence by the primary.
