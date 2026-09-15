# THE-608 diagnostic acceptance — 2026-09-15

The original merge-drain diagnostic-column defect is repaired on local main
`393473624691b0aee04b2725b762e06210c51609`. Independent source and retained
native evidence review found no further scoped production fix. The current05
full-source native gate passed; final metadata landing and tracker closure
remain pending. This record does not mark the shared change or Linear issue complete.

The [evidence manifest](the608-2026-09-15/manifest.json) preserves the exact
original independent review, fresh issue snapshot and source/test map. It links
existing full native and actual CLI receipts without duplicating or rewriting
them. Original payloads have `.raw` suffixes so formatters cannot alter their
bytes. This documentation update performed no test, CLI, provider, live queue,
merge, cleanup or push operation.

## Corrected behavior

The driver constructs typed outcome fields: ordinary reasons/logs enter
`error_detail`, only newline-separated conflict paths enter `conflict_paths`,
and full success object IDs enter `result_oid`. The conflict helper separates
submodule diagnostic prose from its actual path list. Gate infrastructure errors
produce `gate_error` and do not blame the branch or enter agent repair.

The additive `replace_merge_status` writes all nullable outcome fields in one
existing-row SQLite UPDATE. None clears to SQL NULL, removing stale conflicts,
errors and object IDs when folding or a later outcome supersedes them. Nomination
identity, queue time and agent attempts are preserved. Explicit retry clears
outcomes and resets its documented attempt state. The legacy `update_merge_status`
continues to use COALESCE, retaining its None-means-unchanged contract.

Panel progress consumes the same exact nullable fields instead of interpreting
a short display message. Gate-error-only and mixed summaries remain holds/failures,
not successful empty drains. Best-effort driver persistence is still not a
Git/SQLite transaction or a concurrent queue ownership guarantee.

## Source and native evidence

Seven scoped source hashes match the reviewed native623 source exactly:
`merge_driver.rs`, `merge_driver_status_tests.rs`, `handlers/merge_queue.rs`,
`daemon/service.rs`, core `db_aux.rs`, `db_merge_status_tests.rs` and
`store/worktree_aux.rs`. The full paths and SHA256 values are in the manifest.
The independent review verified nine unique native PASS selectors:

- Four core SQLite regressions cover exact status replacement, conflict/retry/error clearing, null versus empty versus legacy None, and missing-row/SQL-failure behavior.
- Three actual driver/SQLite/panel regressions cover conflict-path versus submodule prose, diagnostic-only errors, and full object-ID projection.
- The authenticated daemon transport regression still requires a real private gate-error row with no conflict paths or resultOID, no agent blame/provider execution, and preserved nomination/repository state.
- The actual panel drain-summary regression checks gate-error-only and mixed outcomes.

The retained full623 run passed 8,376 of 8,377 executed tests, with one unrelated
host-key fixture failure and 26 configured skips. That failure and the separate
corrected six-test gate remain recorded in the prior evidence directory. It is
not a clean full-run pass. The historical shared 8,158-test pass remains evidence
for its earlier source, rather than a replacement for current verification.

The completed current native gate at
`33c0db71a3efd91a5637be47fd2d353fd628056a` passed all 8,465 executed tests,
with 26 configured skips, in 337.522 seconds (exit0). Independent review verified
the original log SHA256, all nine exact diagnostic PASS selectors and equality
of the seven diagnostic source files against that Git commit, canonical393 and
this documentation candidate. The manifest retains the original receipt and a
clearly labeled selective PASS excerpt; it records the full original log hash
without presenting the excerpt as a complete log. This result resolves the
current-native pending condition in the original preserved review payload.

## Fixed normal CLI scope

The existing Part B receipts contain four normal `merge drain --json` invocations
of application SHA256
`205941c3b3b0f9b19e7743eb7beed9d117e428dd0554d92be36b5a3934b03918`.
Each exited successfully, retained its exact child through reap, reported no
uncertain cleanup, and removed its owned private root. They seed stale outcome
columns, produce real Git conflict/red-gate cases, assert `agent_blocked` clears
old result/conflict fields and stores only the hold diagnostic, then prove later
same-row readiness after explicit private branch repair. The CLI dispatch calls
the same tested merge driver. Its source is tied to the reviewed combined build;
the focused receipt explicitly does not claim a separately retained embedded
623 version string.

These are layered proofs, not one end-to-end assertion for every variant. Exact
terminal deferred-conflict paths are checked through the shipping driver and
real SQLite with an injected upstream fold result. The normal CLI conflict case
ends in `agent_blocked`; actual gate-error field placement is checked through
native authenticated transport. Neither a live provider nor a real-user queue
operation is inferred. The accepted issue scope requires no new whole-UI or
live-queue exercise.

## Remaining reconciliation

Review and land this metadata with the completed current full-source result,
then reconcile the issue state. Preserve the historical
shared fixed-CLI/private-native/live-queue checkbox and THE-610 obligations;
THE-608's scoped proof does not complete those separate claims or authorize
THE-586 live retry. Existing reciprocal delivery ownership remains unchanged,
and the shared change stays active and verification-gated.
