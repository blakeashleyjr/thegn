# THE-228 plan

Scope: add agent_attempts=0 to both merge_queue enqueue upserts in db_aux.rs (enqueue_merge, enqueue_merge_if_worktree_matches). All callers are deliberate user/API enqueues; persist_merge_outcome already preserves the counter.
Tests: db_merge_status_tests.rs local+remote re-enqueue seeded with attempts and prior detail.
Out of scope: THE-220 claim fencing (unlanded), new typed op.
