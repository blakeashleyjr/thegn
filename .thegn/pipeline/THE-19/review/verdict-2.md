# THE-19 security / test / bug review

FAIL

## Blockers

1. **P1 — hook completion can block forever on an inherited pipe.**
   `crates/thegn-host/src/hook_run.rs:120-121,234-237` unconditionally joins
   the stdout/stderr reader threads after the direct shell exits. A hook such
   as `sleep 600 &` returns from `sh -lc` while the background child inherits
   the pipe, so `join_pipe` waits until that unrelated process exits. The
   configured hook timeout has already stopped being enforced, and the
   lifecycle worker never sends its completion/waker pulse. Process-group kill
   only runs while `try_wait` still sees the direct child alive, so it does not
   close this gap. Add a bounded pipe-drain strategy and a regression test for
   a background descendant before merge.

2. **P1 — detached `session_end` races destructive teardown.**
   `session_end_once` removes the latch and detaches `spawn_session_event` at
   `crates/thegn-host/src/worktree_lifecycle.rs:955-973`; a subsequent
   `destroy_one` at `:633-636` sees no latch and proceeds with pre-destroy,
   runtime teardown, and directory removal while the session-end hook may
   still be running in the worktree. This is reachable by closing the last tab
   (or a pane-exit boundary) and immediately deleting the worktree. It can
   invalidate the hook cwd and reorder user-visible side effects. Track the
   in-flight session event and make destroy await its completion off-loop (or
   otherwise serialize the ownership transition), with a race regression test.

3. **P1 — worker creation errors are swallowed on user-facing async paths.**
   `spawn_event` ignores `Builder::spawn` errors at
   `crates/thegn-host/src/worktree_lifecycle.rs:393-459`. The caller receives
   success for non-waiting post-create/session-start work even when no worker
   exists, and a session-start latch can remain claimed without a completion or
   visible failure. The destroy workers use panic-on-failure `thread::spawn`
   at `:473` and `:533`, which can terminate the host instead of reporting the
   lifecycle operation's failure. Return/propagate a spawn error (and release
   the relevant latch/claim) and add failure-path coverage.

## Review fixes committed

- `0a452783` — avoid `Instant` overflow for a maximum configured hook timeout;
  includes a regression test.
- `63954c62` — release session latches when explicitly closing or keep-files
  removing worktree groups; includes latch regression coverage.
- `e04fe8d5` — required merge of `main` into the review branch, including the
  resolved documentation conflict.

## Verification

- `git diff --check main...HEAD`: passed.
- `just quick thegn-core`: passed before review fixes.
- `cargo nextest run -p thegn-core hooks`: 8 passed after fixes.
- `cargo clippy -p thegn-core --tests -- -D warnings`: passed after fixes.
- `cargo clippy -p thegn-svc --tests -- -D warnings`: passed after fixes.
- `cargo nextest run -p thegn-svc --test control_schema`: passed after fixes.
- `cargo check -p thegn-host --tests`: passed after fixes (7m21s).
- Focused host nextest and host clippy test-target builds were stopped at the
  ten-minute scoped-build limit before producing test output.
- Pre-commit treefmt passed on both review-fix commits. Direct `treefmt
--fail-on-change` could not open its read-only cache; `openspec validate
--all --strict` was unavailable because `openspec` is not installed.
- No live-state `thegn` invocation, migration, e2e, or integration command was
  run. `XDG_STATE_HOME` was isolated for all attempted test commands.

## Snapshots

No frame-affecting changes were made; no snapshot update is required.
