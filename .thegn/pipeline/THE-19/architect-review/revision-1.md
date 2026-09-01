# THE-19 architect revision 1

The implementation needs another pass before it is landable. The fixes below
must preserve the existing off-loop execution and notification seams.

## 1. User worktree deletion must wait for `pre_destroy`

`crates/thegn-host/src/run.rs:1890-1920` runs the sidebar delete hook with
`HookExecutionMode::Force` for every normal delete, so a blocking user hook can
never veto the operation. More importantly, `run.rs:1978-1988` removes the
session panes, DB rows, and in-memory group immediately after spawning the
worker, before either the pre-hook or filesystem removal completes.

Change the normal confirmation path to use user mode and retain the worktree
row/group until the pre-hook succeeds. Report the failure through the existing
refresh/waker path and leave the target available for retry or an explicit
force-confirmed delete. Only prune panes/session/DB state after the requested
physical removal succeeds; keep-files must still run the lifecycle contract
without destroying the directory. The `waker == None` branch at
`run.rs:1962-1975` must use the same lifecycle decision rather than bypassing
hooks.

## 2. Workspace removal needs per-worktree transactional outcomes

`crates/thegn-host/src/handlers/workspace_remove.rs:60-96` forces
`pre_destroy` and proceeds even when it fails. `remove_workspace` then calls
`remove_workspace_with_db` at `workspace_remove.rs:137-145` immediately after
spawning the destructive worker, so failed paths are falsely removed from the
source-of-truth cache and cannot be retried. This also loses the required
per-path failure report for a bulk operation.

Make each path an independent background job with the authorized mode from the
workspace confirmation, collect success/failure back on the loop, and prune
only paths whose requested disk operation completed. Keep failed paths visible
and report all failures; preserve the home checkout guard and do not run
destroy hooks in the keep-files arm.

## 3. Wire `session_end` to actual session boundaries

`crates/thegn-host/src/worktree_lifecycle.rs:345-364` provides
`session_end_once`, but `crates/thegn-host/src/handlers/close.rs` contains no
call when the last tab is closed and `crates/thegn-host/src/pty_drain.rs`
contains no call when the last pane exits. The only current call is the
worktree-delete path (`run.rs:1849`), which does not satisfy the documented
session boundary.

Invoke the once-only, warn-only session-end event after the last pane/tab for a
worktree is actually gone, using the existing background/waker completion
pattern. Keep daemon attach/detach semantics distinct and ensure a failed
session hook cannot block loop-side close bookkeeping.

## 4. Keep the core policy resolver substrate-free

`crates/thegn-core/src/hooks.rs:380-400` exposes `resolve_for_repo`, but it
calls `config::load_repo_overlay` at line 387. That performs filesystem
configuration discovery from inside the core policy seam, contrary to the
design's requirement that resolution be pure and substrate-free.

Move repo-overlay loading to the host/config boundary and pass typed overlay
data into a pure core resolver (or make the existing pure `resolve` the only
core API). Add a unit test that resolves the same inputs without filesystem
access.

## 5. Use lifecycle-aware rollback for failed creates

`crates/thegn-host/src/cmd/wt.rs:275-293` rolls back failed `worktree add` and
DB registration with raw `worktree::remove`, bypassing the shared destroy
lifecycle and its force/diagnostic semantics. Route both rollback cases through
one internal force-cleanup path that runs the applicable destroy hooks,
attempts cleanup off-loop where applicable, and reports hook/cleanup failures
without masking the original create error.

## 6. Sync the OpenSpec contract and runtime behavior

`openspec/changes/add-worktree-lifecycle-hooks/tasks.md:5-68` remains entirely
unchecked and the change is unchanged in this branch, despite the new code.
Update/archive the change so it reflects the shipped implementation and its
remaining obligations, including trust plumbing, doctor/help documentation,
session boundaries, and smoke coverage. In particular, the OpenSpec requires
per-worktree event-indexed logs at
`$XDG_STATE_HOME/thegn/hooks/<slug>/<event>-<n>.log` and a failure notification
with the output tail; `crates/thegn-host/src/hook_run.rs:146-187` currently
creates one hash-named log, while `worktree_lifecycle.rs:386-402` records only
`result.summary()` and omits the captured tail. Align the implementation and
spec, including bounded/redacted tail behavior.
