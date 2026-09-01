# THE-19 architect revision 2

The required scoped gates are green, but the runtime still has semantic gaps
against `architect/design.md`, the OpenSpec workspace contract, and the
revision-1 completion claim. Address these before approval.

## 1. Async destroy workers skip provider/placement teardown

`crates/thegn-host/src/worktree_lifecycle.rs:542-580` runs `pre_destroy`
directly into git removal and `purge_worktree_files`. It never performs the
provider sandbox teardown, VPN/projector cleanup, checkpoint, bridge disconnect,
or placement release that the old sidebar path performed. The post-completion
helper at `crates/thegn-host/src/run.rs:1694-1712` is too late and does not call
`destroy_provider_sandbox` or placement release. The same omission affects bulk
workspace deletion (`handlers/workspace_remove.rs:46-57`) and merge reclaim
(`merge_lifecycle.rs:191-214`).

Move the existing provider/placement teardown into the shared off-loop destroy
transaction, after a permitted `pre_destroy` and before physical removal. Use
the selected environment/config for the target while its path and DB row still
exist. Run `post_destroy` only after removal succeeds, and retain the existing
failure reporting/row-retention behavior when teardown or removal fails. Do not
double-run cleanup from the loop-side cache-prune helper.

## 2. Sidebar deletion has no force retry path

`crates/thegn-host/src/menu.rs:373-398` and `:508-540` produce only delete,
keep-files, and cancel choices. `run.rs:15066-15097` maps both destructive
confirmation surfaces to the same `ConfirmDeleteWorktrees` value, and
`run.rs:1822-1829` always invokes `delete_groups_with_mode` with
`HookExecutionMode::User`. Therefore a failing global/workspace `pre_destroy`
leaves the group visible but offers no way through this UI to choose the
required force/delete-anyway operation; retrying the same choice just runs the
same veto again.

Add an explicit force/delete-anyway state to the existing confirmation/retry
surface (without inventing a new public action id), pass `Force` only for that
explicit choice, and keep the normal choice in `User` mode. The failure must
remain visible and retryable, while repo hooks remain warn-only.

## 3. Vanished-tab reconciliation re-enters destruction

`crates/thegn-host/src/merge_lifecycle.rs:239-266` uses
`delete_groups_with_mode` for `reconcile_removed_tabs`, which now starts a
fresh lifecycle worker. This path is specifically for a worktree already
removed out-of-band. `spawn_worktree_destroy` then calls
`main_worktree(&worktree)` and reports failure when the path is gone, so the
stale session group is not pruned. It also violates the design requirement
that reconciliation must not run hooks or repeat physical removal.

Restore a pure loop-side prune helper for already-vanished groups (pane/session
and cache reconciliation only) and call it from `reconcile_removed_tabs`.
Keep lifecycle hooks exclusively on paths that still own the physical removal.

## 4. `session_end` is scheduled after destruction and after the worktree cwd is gone

`crates/thegn-host/src/worktree_lifecycle.rs:542-580` performs `pre_destroy`
and physical removal before any session-end event. The success handlers at
`worktree_lifecycle.rs:99-100` and `:130-131` call `session_end_once` only
afterward, when a worktree-based hook cwd may no longer exist. A live sidebar
worktree therefore cannot receive the documented session boundary before its
destroy hooks, and the hook commonly fails with a missing cwd.

Close/end the live worktree session at the destroy boundary before
`pre_destroy` (or otherwise schedule the warn-only end event while the cwd is
still valid), then run the destroy transaction. Preserve non-blocking close
semantics and ensure merge/bulk cleanup uses the same ordering. Do not use a
post-removal `session_end` as the only boundary signal.

## 5. Hook environment admits inherited secret-shaped `THEGN_*` variables

`crates/thegn-host/src/hook_run.rs:130-132` calls the general
`filter_host_env`, whose `THEGN_` prefix allowlist is defined at
`crates/thegn-core/src/util.rs:243-252`. That admits inherited variables such
as `THEGN_INBOX_SECRET` or `THEGN_API_KEY`, contradicting the hook contract's
“curated base plus exactly five context values” and the no-secret guarantee.

Add a hook-specific pure environment filter (or a deny pass) that removes
credential-shaped names, including secret-shaped `THEGN_*`, and only then adds
the five `HookContext::environment` values. Add a regression test with
`THEGN_INBOX_SECRET`, `THEGN_API_KEY`, and an agent socket alongside `GH_TOKEN`.

## 6. Wizard `wait=true` repo hooks do not gate the first pane

At `crates/thegn-host/src/wizard.rs:1345-1348`, `schedule_post_create` is
called with `db = None`. Its first resolution at
`worktree_lifecycle.rs:334-340` therefore deny-all treats repo hooks as
pending; an approved repo entry with `wait = true` is not seen by
`waits_for_pane`, and the function schedules it asynchronously instead of
gating the wizard's `Done` event. This is especially observable because the
wizard is the only create path with a custom `WorkerCtx::db_path` test seam.

Pass the worker's active DB/approval source (including the custom test DB when
present) through the post-create call, or provide an equivalent host-bound
approved-policy resolution. Add a test proving an approved repo `wait=true`
entry completes before the wizard emits `Done`.

## 7. Several create failures still leak a speculative worktree

The wizard registers the git worktree before provisioning, but
`wizard.rs:1248-1259` returns on the first registration/DB-open failure without
calling `rollback_remove`. Issue-panel dispatch similarly adds the worktree at
`handlers/tracker.rs:287-290` and returns on later launch-spec/registration
failures without shared rollback. `rollback_remove` itself ignores the
cleanup result at `worktree_lifecycle.rs:651-665`.

Route every post-`worktree add` failure in these create paths through one
force-cleanup transaction, and surface cleanup failure without replacing the
original create error. The successful-create/failed-provision paths must not
leave an unregistered or unusable checkout behind.

## 8. Windows does not use the existing process-tree seam

`crates/thegn-host/src/platform/mod.rs:22-36` makes hook process-group setup a
no-op and timeout cleanup a direct-child kill on non-Unix platforms, while
`platform/windows.rs:174-234` already provides the Job Object seam used by
other workers. The documented timeout contract requires process-tree cleanup,
not just the shell child, on every supported platform.

Route hook spawning/timeout cleanup through the platform's grouped-process
seam, including the Windows Job Object implementation, and add a platform
specific test or documented degraded behavior if that cannot be guaranteed.
