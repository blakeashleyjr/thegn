# Design

The store records one consistent pre-fold registry/queue observation. A short
write transaction compares current state and publishes only the final row,
preserving nomination time/attempt budget and explicitly clearing obsolete NULL
fields. Observable registry or queue reassignment refuses the delayed result.

The host projects the complete report before writing anything: duplicate or
contradictory worktree categories and missing observations reject the projection.
Both CLI and interactive integration share one observation-before-fold wrapper.
Observation failure is visible degraded bookkeeping; it never causes a fresh
post-fold guard. Final write/refusal errors produce no lifecycle for that row.
Successful Landed rows receive Landed lifecycle; all held outcomes receive the
settled Failed folder event. There is no transient Enqueued event.

## Explicit limits

- A snapshot is not a durable ABA generation, lease or exactly-once receipt.
  Identical delete/reinsert and A→B→A changes are not detected.
- Atomicity is per queue row. Earlier rows may commit before a later row refuses.
- SQLite does not atomically cover Git advancement or filesystem lifecycle.
  Post-commit reassignment remains subject to separate lifecycle/Git ownership
  controls; no database writer lock is held across filesystem actions.
- Existing enqueue/retry APIs keep their historical behavior. No schema migration
  or broad mutation API is introduced.
