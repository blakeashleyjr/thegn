# THE-689 plan

Scope: worktree_lifecycle.rs + session.rs only.

1. Race 1: session_start_once checks the physical destroy claim and inserts the latch under the session-runtime lock; refuses with an io error when a destroy holds the claim. Destroy guards read the latch after claiming, so one side always loses.
2. Race 2: process-local tombstone of removed worktree paths set in destroy_one_checked after physical removal; Session::write_layout filters tombstoned groups (dir still absent) after clear_session_layout has taken the write lock.
   Out of scope: resurrect-time filtering (issue says unsafe), dirtiness checks (THE-370/374), cross-process tombstones.
   Tests: refused start under claim, guard sees prior latch, stale snapshot after removal, re-created dir and never-removed paths unaffected.

## Scope notes (review follow-up)

- Race 1 is closed for shell panes only (those routed through session_start_once); split-with-command, agent and resurrect panes, and a cross-process CLI landing, are not covered.
- Both the admission check and the removal tombstone are per-process.
- Tombstones are cleared on create/register and on session start, skip remote rows, are capped at 1024, and are pruned when the path exists again.
