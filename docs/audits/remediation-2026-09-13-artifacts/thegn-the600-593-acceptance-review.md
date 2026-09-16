# THE-600 / THE-593 acceptance follow-up — September 14, 2026

The landed implementation was reviewed against both complete Linear issue
descriptions. Earlier namespace-affected failures are superseded by the exact
native pass receipts in `/tmp/thegn-native-unblocked-host-results.json`.

THE-600's initial malformed registry and unknown resource cases already passed,
as did its real post-hook unknown-dispatch case. The remaining explicit gate was
registry decoder failure after initial valid admission. THE-593 already passed
selection requeue/revocation, actual committed-outcome and manual clearing tests;
the added gate isolates a result-only refinalization and documents the existing
same-value ABA proof limit.

## Reviewed follow-up commits

- THE-600 `db2dbb55`: retain the existing unknown-dispatch regression and share
  its private hook rendezvous with a new malformed-registry case. The exact row
  initially decodes. Only after pre_destroy starts does a second SQLite
  connection corrupt position, with exactly one affected row. Revalidation must
  report the actual decoder error, while no dispatch hold can mask the path.
  Refusal preserves worktree contents, branch refs and queue state. The already
  authorized hook remains observed. Scoped thread/release ownership and finite
  deadlines are retained.
- THE-593 `7ad070c6`: capture a landed row, create a real descendant target commit,
  and change only result_oid through a second connection. Full-row equality
  proves every other observed value remained unchanged. Actual production
  cleanup reaches the selected-row mismatch, refuses before pre_destroy and
  preserves contents/refs/queue. The same stale row cannot authorize manual
  clearing. Mixed asynchronous clearing retains refinalized rows and counts
  only actual deletions, even with duplicate selection.

These are test/specification changes, with no new production hooks, schema,
durable generation, queue retry, runtime teardown or live state mutation.
Same-value ABA restoring all observed fields remains explicitly outside the
value-comparison guarantee.

## Review and evidence status

Primary approved both bounded production-seam plans before edits. The independent
BTOP/authority lane reviewed both stable commit IDs and found no remaining source
blocker. Formatting, diff and strict OpenSpec validation passed locally.
Compiled regression execution remains the final gate at this report checkpoint;
the root's full test run is in progress. Do not mark execution or issue closure
from source approval alone.

The first September 14 full workspace run subsequently executed and passed all
four strict core reader/resource regressions: raw tenancy associations, unknown
and pending dispatch state, worktree-record malformed/null-default decoding, and
worktree-record query failure distinct from absence. Exact PASS lines appear at
1630–1797 of `/tmp/thegn-rolling-full-just-test-20260914.log`. That run later failed
at an unrelated logging formatter test and never reached the host follow-ups;
these individual core passes are not a claim that the workspace gate passed.

## Final host execution observed

Both stable source candidates executed in the assembled no-fail-fast workspace
run `/tmp/thegn-rolling-full-workspace2-20260914.log`. THE600's existing
unknown-dispatch case passed at line6346 and the new post-admission malformed
registry case passed at line6348. THE593's mixed exact-row manual clear passed at
line6337, new refinalized result-OID regression at line6342, and selected-row
revocation regression at line6343. These are actual host binary passes, following
the source approvals at `db2dbb55` and `7ad070c6`.

The entire run completed 8354 tests with 8348 passes and 6 failures, plus26
configured skips. The six failures concern configuration documentation/env
coverage, Git fixture setup, and watchdog fixture isolation. They do not invalidate
these exact passes, but the overall workspace gate remains failed pending repair
and rerun. No same-value ABA generation claim is added.
