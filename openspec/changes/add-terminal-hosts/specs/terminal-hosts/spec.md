# Terminal Hosts

## ADDED Requirements

### Requirement: Terminals are first-class sidebar groups outside git

superzej SHALL manage terminal environments (local, ssh, mosh, container) persisted in a `terminals` table and rendered as a Terminals section in the sidebar, where selecting a terminal row behaves like a worktree row (activate the group, show its tabs, spawn if not running); git-only queries (PR counts, branch) MUST return empty/None for terminal groups rather than erroring.

#### Scenario: Select a terminal row

- **WHEN** the user selects a terminal row
- **THEN** that terminal becomes the active group, its tabs appear, and a session
  spawns if none is running

#### Scenario: Git queries are safe for terminals

- **WHEN** a git-dependent query runs against a terminal group
- **THEN** it returns empty/None without raising an error

### Requirement: Connection kind drives the spawned process

A terminal's `kind` SHALL determine the spawned process: `local` drops into `$HOME`, while `ssh`/`mosh` exec the connection binary instead of `$SHELL`, degrading gracefully when the binary is unavailable.

#### Scenario: Remote terminal connects

- **WHEN** an `ssh` terminal is opened
- **THEN** the pane execs `ssh <connection>` rather than a local shell

#### Scenario: Missing connection binary

- **WHEN** the connection binary for a terminal is not installed
- **THEN** an error is shown rather than crashing the session
