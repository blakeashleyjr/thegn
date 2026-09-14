## ADDED Requirements

### Requirement: Snapshot only selected and confirmed local candidates

Candidate discovery MUST NOT create snapshots. Queue selection and confirmation
MUST precede snapshot mutation. The CLI and UI MUST use the same selected-only
admission and MUST refuse unknown dirty state or changed local Git identity.

#### Scenario: A dirty unqueued worktree exists

- **WHEN** integration selects only queued worktrees with snapshotting enabled
- **THEN** unqueued worktree refs, index and contents remain untouched.

#### Scenario: Dry-run or missing confirmation

- **WHEN** the CLI previews or refuses an unconfirmed noninteractive integration
- **THEN** neither queued nor unqueued worktrees are snapshotted, the queue is
  unchanged, and missing confirmation returns a nonzero exit status.

#### Scenario: Snapshot identity changed

- **WHEN** a selected worktree changes branch, HEAD, repository or placement
- **THEN** snapshot admission refuses instead of resolving a new mutation target.
