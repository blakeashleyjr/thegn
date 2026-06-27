# State Database

## Purpose

superzej persists session and UI state in a local SQLite database so a restart
can resurrect the exact working context. The DB is a cache and resurrection
layer over git (the source of truth), with a versioned schema that migrates
forward deterministically.

## Requirements

### Requirement: Single versioned SQLite store

Persistent state SHALL live in a single SQLite database at `$XDG_STATE_HOME/superzej/superzej.db` in WAL mode with the schema version tracked via SQLite `user_version`, and any schema change MUST bump `user_version` and provide a forward migration.

#### Scenario: Forward migration on open

- **WHEN** the host opens a DB whose `user_version` is older than the current
  schema
- **THEN** the migrations run in order to bring the schema up to date before use

### Requirement: DB is a cache, not the source of truth

The database SHALL function as a cache and resurrection layer, and git MUST remain authoritative for worktrees such that the DB never contradicts git's view.

#### Scenario: Resurrection from cache

- **WHEN** the host restarts
- **THEN** it restores the prior workspace/worktree/tab/pane context (including
  pane working directories) from the DB, reconciled against git's actual state

### Requirement: Test isolation of state

Any test or benchmark that opens the DB or spawns the host SHALL isolate `XDG_STATE_HOME` so it MUST NOT read or mutate the user's real session state.

#### Scenario: Isolated test run

- **WHEN** a test that touches the DB runs while a live superzej session exists
- **THEN** it uses an isolated `XDG_STATE_HOME` and leaves the real database
  unchanged
