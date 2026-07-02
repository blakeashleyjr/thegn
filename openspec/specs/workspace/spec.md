# workspace Specification

## Purpose

TBD - created by archiving change add-workspace-create-delete. Update Purpose after archive.

## Requirements

### Requirement: Create a workspace from a path or git URL

The native host SHALL let the user create a workspace by entering a local path or a git URL, cloning a URL into the workspaces directory and validating a path as an existing git repository before registering the workspace and switching to it, and MUST reject invalid input (empty, non-existent path, or failed clone) with a clear error rather than registering a broken workspace.

#### Scenario: Create from an existing repo path

- **WHEN** the user enters a path to an existing git repository
- **THEN** the workspace is registered and the session switches to it

#### Scenario: Create from a git URL

- **WHEN** the user enters a git URL
- **THEN** the repository is cloned into the workspaces directory, registered, and switched to

#### Scenario: Directory that is not a git repository

- **WHEN** the user enters a path to a directory that is not a git repository
- **THEN** the host offers to initialize a git repository there before registering it

#### Scenario: Invalid path is rejected

- **WHEN** the user enters a path that does not exist
- **THEN** an error is shown and no workspace is registered

### Requirement: Create a workspace from an auto-discovered repo

When repositories exist under the configured root directories, the native create action SHALL present them for selection so the user can register one without typing its path, while still offering a way to type an arbitrary path or URL.

#### Scenario: Pick a discovered repo

- **WHEN** the user triggers the create action and repositories are discovered under the configured roots
- **THEN** a picker lists those repositories and selecting one registers and switches to it

#### Scenario: No repos discovered

- **WHEN** the user triggers the create action and no repositories are discovered
- **THEN** the host opens the path-or-URL entry prompt directly

### Requirement: Delete a workspace with confirmation, keeping files optional

Deleting a workspace SHALL require confirmation and MUST remove the database registration while leaving the workspace's worktree files on disk when the user chooses to keep them, reporting how many worktrees remain, and MUST switch to the next available workspace or fall back to an empty home when none remain; a destructive option that also deletes the branch worktree directories from disk MAY be offered as an explicit confirmed choice.

#### Scenario: Keep files reports orphaned worktrees

- **WHEN** the user confirms deletion of a workspace with the keep-files option and the workspace still has registered worktrees
- **THEN** the registration is removed, the worktree files on disk are untouched, and the status reports how many worktrees remain on disk

#### Scenario: Delete the last workspace

- **WHEN** the user deletes the only remaining workspace
- **THEN** the session falls back to an empty home rather than leaving no context
