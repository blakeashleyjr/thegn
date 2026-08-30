# THE-19 revision 3 — architect findings

## 1. Issue-panel dispatch leaks on `git worktree add` failure

`crates/thegn-host/src/handlers/tracker.rs:287-290` returns immediately when
`wt::add_checked` fails. This is a create path named by the design call-site
map, and `git worktree add -b` may have created part of the checkout/branch
before reporting an error. The other failures in this path use the lifecycle
rollback seam, but this first post-hook failure does not.

Route this error through the shared force rollback transaction. Preserve the
original `git worktree add` error as the primary diagnostic and append/report a
rollback failure rather than hiding either condition. Add a regression test
covering a failed/partial add so the issue dispatch path cannot leave an
unregistered checkout or candidate branch behind.

## 2. Control `worktrees.create` has the same add-failure leak

`crates/thegn-host/src/daemon/service.rs:1378-1379` maps an
`wt::add_checked` error directly into the control error and exits. This is the
fourth create path explicitly required to share the lifecycle behavior. It
must use the same force rollback/error-preservation contract as the wizard,
CLI, and issue-dispatch paths, with a regression test at the control seam.

## 3. Vanished-tab reconciliation performs blocking I/O on the loop

`crates/thegn-host/src/merge_lifecycle.rs:214-265` is called on the event loop,
but opens and queries SQLite (`:218-227`), stats every worktree path with
`Path::is_dir` (`:228-236`), opens SQLite again, writes cache rows, and
persists the session (`:247-264`). The comment calls the stat “cheap”, but it
is still blocking filesystem I/O and the database operations are unambiguously
blocking. This violates the repository's 0% idle standard and the design's
off-loop lifecycle rule.

Move the source-of-truth probing and cache/session writes to an off-loop
worker. Return a typed reconciliation completion through the existing refresh
channel; keep the loop-side handler limited to pruning the already identified
vanished groups and applying the resulting model/focus update. Add a test or
seam assertion that the loop handler does not open/query SQLite or perform
filesystem I/O.
