# Workspace

## ADDED Requirements

### Requirement: Create a workspace from a path or git URL

The native host SHALL let the user create a workspace by entering a local path or a git URL: a URL is cloned into the workspaces directory and a path is validated as an existing git repository, after which the workspace is registered and switched to; invalid input (empty, non-existent path, non-repo, or failed clone) MUST produce a clear error rather than registering a broken workspace.

#### Scenario: Create from an existing repo path

- **WHEN** the user enters a path to an existing git repository
- **THEN** the workspace is registered and the session switches to it

#### Scenario: Invalid path is rejected

- **WHEN** the user enters a path that does not exist or is not a git repository
- **THEN** an error is shown and no workspace is registered

### Requirement: Delete a workspace non-destructively with confirmation

Deleting a workspace SHALL require confirmation and MUST remove only the database registration — worktrees on disk are left intact — then report how many worktrees remain orphaned and switch to the next available workspace, creating an empty home when none remain.

#### Scenario: Delete warns about orphaned worktrees

- **WHEN** the user confirms deletion of a workspace that still has registered
  worktrees
- **THEN** the registration is removed, the worktrees on disk are untouched, and
  the status reports the orphaned worktree count

#### Scenario: Delete the last workspace

- **WHEN** the user deletes the only remaining workspace
- **THEN** the session falls back to an empty home rather than leaving no context
