# THE-733 plan

Scope: stop a daemon-pane exit stamping a session-less dispatch row it does not own.

- core: `Db::bind_dispatch_session` (active + session-less only, bumps run_gen); delete the session-less fallback in `dispatch_for_exit` (non-empty sid miss = Stale).
- host: tracker dispatch records its row id on `CreatedWorktree.dispatch_id`; the loop maps pane id -> row (`Panes.dispatch_panes`); pane exit binds the announced session before attribution.
- tests: db_tests (plain shell Stale, bind then Exact, bind refusals).
  Out of scope: binding at first output (session id is announced async); no schema change.
