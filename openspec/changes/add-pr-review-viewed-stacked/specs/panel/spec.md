# Panel

## ADDED Requirements

### Requirement: A reviewer can mark PR files viewed, persisted and synced to GitHub

The PR panel SHALL let a reviewer mark a file as viewed, persist that state
locally keyed by worktree + PR + file path, and sync it to GitHub's native
per-file "viewed" flag, so review progress survives a restart and matches the
GitHub web UI. Marking MUST update the local cache and UI immediately and sync to
GitHub off the event loop; a sync failure MUST degrade to local-only without a
panic, and a `user_version` bump provides the viewed-state table.

#### Scenario: Viewed state persists across restart

- **WHEN** a reviewer marks a file viewed and later restarts superzej
- **THEN** that file is still shown as viewed in the PR panel

#### Scenario: Viewed state syncs with GitHub

- **WHEN** a reviewer marks a file viewed and GitHub is reachable
- **THEN** the file's GitHub "viewed" flag is set, and on next refresh GitHub's
  viewed set reconciles into the local cache

#### Scenario: Offline marking still works

- **WHEN** a reviewer marks a file viewed while GitHub is unreachable
- **THEN** the local viewed state is recorded and shown, with no error

### Requirement: A PR can be reviewed commit-by-commit (stacked)

The PR panel SHALL provide a stacked review mode that walks a PR one commit at a
time, rendering each commit's own diff (the range from its parent to itself)
instead of only the squashed whole-PR diff, with a toggle back to the squashed
view. The squashed whole-PR diff MUST remain the default view.

#### Scenario: Stacked mode shows a single commit's diff

- **WHEN** a reviewer enables stacked mode and steps to a commit
- **THEN** the panel renders only that commit's diff (its parent-to-commit range)

#### Scenario: Toggling back shows the whole-PR diff

- **WHEN** a reviewer toggles off stacked mode
- **THEN** the panel renders the squashed whole-PR diff
