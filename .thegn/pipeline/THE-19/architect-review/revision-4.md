# THE-19 architect revision 4

## 1. Lifecycle completion handling still performs blocking work on the compositor loop

`crates/thegn-host/src/worktree_lifecycle.rs:77-198` is called from the event-loop refresh path, but the success branches at lines 101-103, 131-134, and 157-171 open the database and perform synchronous database/cache writes. The same handler calls `run::persist_session_layout` at lines 193-195; `crates/thegn-host/src/run.rs:5490-5506` captures live pane state with O(children) `/proc` reads before enqueueing the eventual write. This contradicts the handler's own promise at lines 74-76 that no filesystem work occurs there and violates the repository's 0% idle rule.

Move all `Db::open`, `forget_*`, `remove_workspace_with_db`, session persistence, and live pane-state capture to the worker/db-task side. Return enough plain completion data for the loop to update only in-memory session, pane, model, and focus state. Add a regression/seam test that exercises every successful completion variant without opening the DB or probing pane state from `apply_completions`.

## 2. Hook trust requests are not reachable through the documented approval command

`crates/thegn-host/src/worktree_lifecycle.rs:244-265` resolves pending hook requests and `run_event_with_db` only warns about them at lines 318-323. However, `crates/thegn-host/src/cmd/repos.rs:113-116,118-130,135-163` implements `thegn repo trust` exclusively through `Config::repo_sandbox_resolved`, whose pending list does not include the hook requests. Consequently a repo hook can remain pending and be reported with no way for the user to obtain its request id or approve it via the documented command.

Unify hook and sandbox requests in the repo-trust listing/approval surface (preserving the canonical `hooks.<event>` request identity and the `sandbox.prepare` → `post_create` compatibility rule), or explicitly add hook resolution to this command. Add tests covering list, approve, and subsequent hook execution for a pending repo hook.
