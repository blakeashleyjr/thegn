# Design

With snapshotting enabled, read-only discovery records each linked worktree's canonical local checkout,
common Git directory, per-worktree administration directory, branch and HEAD.
Dirty status failures refuse discovery. With snapshotting enabled, dirty
candidates remain pending rather than becoming commits.

Snapshot-disabled discovery retains its ordinary read-only local tip lookup;
strict canonical-path admission does not become a new restriction on that route.
Windows positive snapshot admission is not established: unsupported path forms
hold rather than normalizing away the alias check.

CLI queue selection precedes the displayed plan, dry-run and confirmation.
Enqueue-all callers only discover and nominate. CLI and UI folds share a helper
that observes queue state before Git mutation, preflights all selected local
identities and registry placement, then snapshots only selected worktrees. The
snapshot operation uses the captured local location, never a new DB/remote
location resolution. Observed identity changes and unsupported placement hold.

## Limits

Git identity rechecks are not a filesystem lock or durable ABA lease. External
concurrent Git mutation remains possible between checks and commands. A later
snapshot failure may leave earlier authorized snapshots and a staged failing
worktree; errors say so and no rollback is attempted. Final queue projection
keeps THE-591's observed-state guard and THE-588 remains a separate cleanup guard.

Dry-run may initialize or prune ordinary application state. Its guarantee is no
candidate or queue mutation, not zero filesystem activity. Private native tests
initialize their database before comparing logical rows and candidate bytes.
The harness explicitly locates XDG/app/Git state without assigning HOME or
CODEX_HOME and disables legacy migration. It admits private database schema
creation only after proving its owned path; no daemon or remote is started.
