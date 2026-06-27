# Git Backend

## Purpose

superzej reads and writes git state through a service seam that prefers fast,
native (gix) reads but always degrades to the git CLI for writes and for any
operation the native path does not cover. git remains the source of truth for
worktrees.

## Requirements

### Requirement: GitBackend trait with native-first reads

Git operations SHALL go through the `GitBackend` trait; reads MUST prefer the gix-native provider for speed and MUST fall back to the git CLI when the native path is missing or errors.

#### Scenario: Native read succeeds

- **WHEN** a supported read (e.g. ahead/behind, status) is requested and the gix
  provider can serve it
- **THEN** the native provider answers without spawning the git CLI

#### Scenario: Native gap falls back to CLI

- **WHEN** a requested operation is not implemented natively or the native call
  fails
- **THEN** the backend transparently falls back to the git CLI subprocess

### Requirement: Writes go through the CLI

Mutating git operations SHALL be performed via the git CLI to match git's exact write semantics.

#### Scenario: Write operation

- **WHEN** a write operation (e.g. commit, branch creation) is invoked
- **THEN** it is executed through the git CLI

### Requirement: git is the source of truth for worktrees

The set of worktrees SHALL be derived from git, and the SQLite DB MUST act only as a cache/resurrection layer that never overrides what git reports.

#### Scenario: DB disagrees with git

- **WHEN** the DB's cached worktree list differs from git's actual worktrees
- **THEN** git's view wins and the cache is reconciled to match
