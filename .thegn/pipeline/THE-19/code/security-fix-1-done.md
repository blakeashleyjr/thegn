# THE-19 security-fix-1 completion

## Fixed

- Waiting, user-scoped `post_create` failures now return a `LifecycleReport` and
  block creation. Wizard, CLI, issue dispatch, and daemon creation paths route
  the failure through lifecycle-aware rollback/error handling.
- `session_start` claims are released when environment resolution, launch-spec
  creation, or pane spawning fails, allowing a retry to run the hook again.
- Close, worktree-close, and workspace-removal loop handlers no longer open or
  persist SQLite state synchronously. Cache/layout work runs through existing
  workers while loop-side reconciliation remains in memory.
- Destructive workspace removal combines live session worktree paths with DB
  paths on the worker, preserving the home-checkout guard and handling
  unregistered live worktrees or unavailable DB state.
- Individual and bulk destruction share an atomic per-path in-flight claim;
  duplicate requests are rejected until the owning worker releases the claim.
- Hook log directory, permission, open, and write errors are returned on
  `HookRunResult`, retained alongside the primary hook result, and surfaced via
  lifecycle notifications. The bounded redacted output-tail behavior remains
  intact.

## Disputed

None.

## Commits

- `7c735784` — surface hook log failures
- `d7831086` — release session-start latch on spawn failure
- `8a1b6c04` — gate creates on waiting post-create hooks
- `67bc0352` — move lifecycle deletion I/O off loop

## Verification

- `cargo nextest run -p thegn-host 'worktree_lifecycle' 'workspace_remove' 'worktree_delete' 'hook_run'` — 32 passed.
- `cargo clippy -p thegn-host --tests -- -D warnings` — passed.
- Pre-commit `treefmt` — passed on all commits.
- `git diff --check` — passed; worktree clean.

## Unverified

- `just quick thegn-host` could not complete because its temporary-directory
  setup initially targeted a read-only `/run/user/1000`; the equivalent scoped
  host clippy check passed with isolated `XDG_STATE_HOME`/runtime settings.
- No full-workspace gate, migration, live-state DB invocation, or e2e run was
  performed, per the revision dev-loop policy.
