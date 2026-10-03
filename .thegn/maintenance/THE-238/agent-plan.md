# THE-238 plan: bind exit stamps to session and run generation

Scope: thegn-core (db, db_migrate, db_dispatch, db_notification, issue, store/notification) and the two host observers (pty_drain, daemon/pipeline_retry).

Approach (approved by Blake, schema v71):

- v71 migration: additive `agent_dispatches.run_gen INTEGER NOT NULL DEFAULT 0`, idempotent via has_column, verified before the version stamp.
- run_gen bumps in every UPDATE that writes session_id: publish_dispatch_run, compare_and_set_dispatch_retry_run, stamp_dispatch_run.
- `stamp_dispatch_exit(id, expected_session, expected_gen, code) -> ExitStamp` is one CAS; Stale and Missing mutate nothing.
- `dispatch_for_exit` returns typed `ExitAttribution`: a non-empty session id is Exact or Stale (no worktree fallback); identity-less events resolve only on exactly one active row, else Ambiguous or NoRow.
- Both observers use the same attribution and CAS and log stale or ambiguous events.
- run_gen stays off `AgentDispatch` (no control-schema change).

Tests: reordered old exit after retry publication, duplicate exits, replaced rows and sessions, reused worktree, simultaneous old and new sessions, ambiguity, idempotent migration preserving active rows.

Out of scope: status-transition CAS beyond the retry publication.
