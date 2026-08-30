# THE-19 chunk 2 completion

Implemented the host-side lifecycle hook runner and shared orchestration seam.

- Added `hook_run.rs` for off-loop `sh -lc` execution through
  `wrap_background_argv`, curated environment filtering, null stdin, captured
  output, per-worktree state logs, timeouts, and process-group cleanup.
- Added `worktree_lifecycle.rs` for policy resolution, cwd contracts, blocking
  versus force/unattended modes, durable failure notifications, rollback, and
  once-per-process/worktree session latches.
- Routed wizard, CLI (`wt new`, issue/project creation, `wt rm`), issue-panel,
  daemon/control creation, sidebar/workspace cleanup, and merge reclaim paths
  through the shared lifecycle events.
- Kept CLI creation synchronous through post-create completion, daemon/UI
  post-create asynchronous, and preserved existing lazy sandbox provisioning.
- Replaced the wizard's separate legacy prepare execution with the core
  compatibility alias handled by post-create.

## Verification

- `just quick thegn-host` — passed.
- `cargo nextest run -p thegn-host hook_run` — 3 passed.
- `cargo nextest run -p thegn-host worktree_lifecycle` — 2 passed.
- `cargo nextest run -p thegn-host merge_lifecycle` — 15 passed.
- `cargo nextest run -p thegn-host worktree_delete` — 10 passed.
- `git diff --check` — passed.
- Checks used `XDG_RUNTIME_DIR=/tmp/thegn-runtime-the19` and
  `RUSTC_WRAPPER=` for this sandbox.

## Unverified

- The prescribed `cargo nextest run -p thegn-host delete_groups` filter selected
  no tests in this branch; the matching `worktree_delete` handler suite passed
  instead.
- Full-workspace gates (`just test`, `just ci`, coverage, and e2e) were not run
  per the chunk and dev-loop policy.
