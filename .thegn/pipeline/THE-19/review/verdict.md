FAIL

THE-19 is not ready for the merge queue.

Findings

1. High — `crates/thegn-host/src/worktree_lifecycle.rs:342-357` discards the
   `LifecycleReport` from synchronous `post_create` execution. A user-scoped
   `post_create` with `wait = true` and `on_failure = block` therefore runs,
   fails, and is then allowed to continue to the first pane. This violates the
   documented wait gate and is a swallowed user-invoked failure. Add a focused
   test that asserts a blocked wait result stops the create pipeline, then route
   that result through each create caller's rollback/error path.

2. High — `crates/thegn-host/src/run.rs:4975-5026` claims the session-start
   latch before `env_halt_reason`, provider resolution, launch-spec creation,
   and pane spawn. Any of those failures leaves the latch set, so retrying the
   same worktree in this host session never runs `session_start` again. Add a
   release-on-spawn-failure path and a regression test for a failed spawn.

3. High — `crates/thegn-host/src/handlers/close.rs:122` and
   `crates/thegn-host/src/handlers/worktree_delete.rs:114,156-169` perform
   synchronous DB open/persist/cache work on the compositor loop. The
   destructive workspace fast path at
   `crates/thegn-host/src/handlers/workspace_remove.rs:90-110` also opens SQLite
   and performs cache pruning on the loop when no DB rows are found. This
   violates the zero-blocking-I/O lifecycle rule and remains untested by the
   completion-only loop seam tests. Move these operations to workers and keep
   loop handlers to in-memory reconciliation.

4. High — destructive workspace removal treats the DB registry as the complete
   set of files to delete (`workspace_remove.rs:27-37,97-110`). A missing DB row,
   a DB-open failure, or a live session worktree not yet registered makes
   `worktree_dirs` empty; the code then reports success and removes only cache
   state while leaving branch checkout directories on disk. Derive the delete
   set from both live session groups and the DB, preserve the home-path guard,
   and test DB-unavailable/missing-row cases.

5. High — `crates/thegn-host/src/run.rs:1823-1856` has no in-flight ownership
   guard. Repeated delete confirmation, individual deletion plus workspace
   deletion, or concurrent refresh actions can start multiple destroy workers
   for one path. They can run lifecycle hooks twice and race git removal,
   purge, and cache completion. Add an atomic per-path claim released by the
   worker and test duplicate requests.

6. Medium — `crates/thegn-host/src/hook_run.rs:309-329` still ignores directory,
   open, chmod, and write failures when recording a hook result. The hook's
   primary result remains visible, but the documented per-worktree log can be
   silently lost with no diagnostic. Return the logging error through the
   result/notification path (without replacing the hook failure) and test a
   read-only/unwritable log target.

Review fixes committed separately:

- `bda219c8 fix(the-19): contain hook output and protect logs (review)` — caps
  pipe capture, avoids raw command logging, redacts common credential-shaped
  output, tightens log permissions, and kills/reaps after `try_wait` errors.
- `dcd6eec fix(the-19): preserve pre-existing branches on add rollback (review)`
  — carries branch-creation state under the git mutation lock and prevents
  rollback from deleting a pre-existing branch.

Verification

- Passed: `just quick thegn-host`, `just quick thegn-core`,
  `just quick thegn-svc` (with `RUSTC_WRAPPER=` and an isolated runtime/state
  directory).
- Passed focused suites: host hook 7, lifecycle 5, merge lifecycle 16,
  worktree delete 10, help 75; core hooks 8, worktree 99, env overlay 2,
  config example 3; service control snapshot 1.
- The prescribed `cargo nextest run -p thegn-host delete_groups` selected no
  tests (exit 4); its available `worktree_delete` equivalent passed.
- `treefmt --fail-on-change` could not initialize because `taplo` is missing;
  pre-commit treefmt passed on both review commits.
- `openspec validate --all --strict` could not run because `openspec` is not
  installed. No migration, live-state DB access, full build, or e2e was run.

Snapshots

No frame-affecting changes were made by the review fixes; no snapshot update is
requested.
