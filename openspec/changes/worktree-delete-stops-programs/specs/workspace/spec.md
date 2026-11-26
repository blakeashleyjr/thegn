# Workspace — worktree delete stops programs delta

## ADDED Requirements

### Requirement: Deleting a worktree stops its programs and refuses unsafe targets

Deleting a worktree from disk SHALL stop the worktree's pane processes after
the `pre_destroy` hook passes and before any teardown removes the folder. Each
process SHALL get a terminate signal, then a kill after a short grace period.
Where the platform can list processes by working directory, the delete SHALL
then wait, for a bounded time, until no other process uses the folder. If a
process outlives that wait, the delete SHALL proceed and SHALL name it in the
result. The branch SHALL be kept.

The repository's main checkout and a worktree locked with `git worktree lock`
SHALL be refused with a plain reason before any process is stopped, any hook
runs, or any tab is closed.

#### Scenario: A running program is stopped before the folder is deleted

- **WHEN** a worktree with a running `sleep` in one of its panes is deleted from disk
- **THEN** the `sleep` has exited, the worktree folder is gone, and the branch still exists

#### Scenario: A locked worktree is refused

- **WHEN** the user deletes a worktree that is locked with `git worktree lock`
- **THEN** the delete is refused with the lock reason and the `git worktree unlock` command, and its programs keep running

#### Scenario: The main checkout is refused

- **WHEN** a delete targets the repository's main checkout
- **THEN** the delete is refused with a message naming the main checkout, and nothing is stopped or closed
