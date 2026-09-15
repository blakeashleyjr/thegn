# Strict standalone state host capture

THE-603 supplies an unwired capture primitive on top of THE-602's strict
host-definition snapshot. The legacy read-only openers conflate absent paths
with inspection failures and suppress version-query failures. Immutable reads
are inappropriate for launch configuration derived from a changing WAL DB.

Add a distinct logically read-only Unix-VFS capture primitive and a private
Linux host inspection helper. Absent is a genuine missing-component observation,
not a fallback for unreadable, unsupported, malformed or incompatible state.
Retain and reverify the observed ancestor chain before publishing absence; any
orphan WAL, SHM or journal object beside a missing base is a typed refusal.

## Impact

Roadmap O188 (configuration diagnostics), A6 (storage seams), and AC363 through
parent THE-592 (launch admission). Depends on THE-602; related THE-598. Core SQL
capture, host platform inspection, private fixtures and reciprocal THE-603
delivery entries. The existing locked rusqlite host dev dependency is reused.
No schema migration, startup/receiver wiring, worker implementation, global policy
install or live DB access. The primitive alone does not close THE-592 or establish
workload isolation, path ownership, or universal platform support.
