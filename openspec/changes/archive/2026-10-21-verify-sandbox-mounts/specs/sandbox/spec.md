# Sandbox

## ADDED Requirements

### Requirement: Bind mounts are verified from inside the container

thegn SHALL confirm that a worktree bind actually delivered files before treating
a sandbox as usable, and MUST NOT infer success from the runtime echoing back the
mount arguments it was given. When a bind is refused at create, the runtime's own
diagnosis MUST reach the user; when a bind silently yields an empty directory,
thegn MUST detect it from inside the container, remove the container so a widened
share can take effect, and fail the backend so the chain falls through.

A verification MUST only assert a path thegn has just observed on the host, so a
sandbox that cannot be proven broken behaves exactly as it did without the check.

#### Scenario: A runtime that refuses an unshared bind explains why

- **WHEN** a container create fails because the runtime cannot resolve a bind
  source (on macOS, a worktree outside the VM's shared directories)
- **THEN** the failure names the missing path and the runtime-specific remedy,
  and no further create is attempted with the same mount set

#### Scenario: A runtime that silently mounts an empty directory is caught

- **WHEN** a container starts but the worktree bind produced an empty directory
- **THEN** the launch fails rather than opening a pane on an empty worktree, the
  container is removed, and the message names the missing path and the remedy

#### Scenario: An unprovable spec is left alone

- **WHEN** nothing was mounted, the placement is not local, or the host does not
  have the path that would be asserted
- **THEN** no assertion is made and the preflight probe behaves exactly as it
  does without mount verification

#### Scenario: The remedy names the share that actually failed

- **WHEN** the missing path is on a different volume than the worktree
- **THEN** the remedy names the missing path's share root, not the worktree's
