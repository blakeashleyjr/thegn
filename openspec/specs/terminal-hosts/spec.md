# terminal-hosts Specification

## Purpose

First-class terminal groups (local, ssh, mosh, container) persisted and rendered in the sidebar alongside worktrees, with connection-kind-driven spawning and graceful degradation.

## Requirements

### Requirement: Terminals are first-class sidebar groups outside git

thegn SHALL manage terminal environments (local, ssh, mosh, container) persisted in a `terminals` table and rendered as a Terminals section in the sidebar, where selecting a terminal row behaves like a worktree row (activate the group, show its tabs, spawn if not running); git-only queries (PR counts, branch) MUST return empty/None for terminal groups rather than erroring.

#### Scenario: Select a terminal row

- **WHEN** the user selects a terminal row
- **THEN** that terminal becomes the active group, its tabs appear, and a session
  spawns if none is running

#### Scenario: Git queries are safe for terminals

- **WHEN** a git-dependent query runs against a terminal group
- **THEN** it returns empty/None without raising an error

### Requirement: A terminal is one shell, reachable from any workspace

A terminal SHALL have at most one live session at a time regardless of which
workspace it is opened from. Because the `terminals` registry is global and its
sidebar row renders in every workspace, activating a terminal MUST reunite it
with its existing session before spawning: reusing the group when resident,
migrating the live group when it is parked with another workspace, and restoring
its persisted layout (so the daemon session reattaches) when neither. Only a
terminal with no live and no persisted session spawns a new one.

#### Scenario: Re-open a terminal from another workspace

- **WHEN** the user opens a terminal in one workspace, switches to another
  workspace, and activates that same terminal row there
- **THEN** the original session is re-shown — same running process, same
  scrollback — rather than a second group with a new shell

#### Scenario: Re-open a terminal with no resident group

- **WHEN** a terminal's group is resident in no workspace (a fresh launch, or a
  workspace evicted from the resident pool) but its layout was persisted
- **THEN** the terminal is restored from that layout so its session reattaches,
  falling back to repainting the persisted scrollback tail when the session is
  gone

### Requirement: Connection kind drives the spawned process

A terminal's `kind` SHALL determine the spawned process: `local` drops into `$HOME`, while `ssh`/`mosh` exec the connection binary instead of `$SHELL`, degrading gracefully when the binary is unavailable.

#### Scenario: Remote terminal connects

- **WHEN** an `ssh` terminal is opened
- **THEN** the pane execs `ssh <connection>` rather than a local shell

#### Scenario: Missing connection binary

- **WHEN** the connection binary for a terminal is not installed
- **THEN** an error is shown rather than crashing the session
