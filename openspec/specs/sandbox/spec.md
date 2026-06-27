# Sandbox

## Purpose

Each worktree's interactive process can run inside an isolation backend so that
untrusted or experimental work is contained, while the worktree itself stays on
the host so host-side git reads keep working. Backend selection degrades
gracefully across the available container/sandbox runtimes.

## Requirements

### Requirement: Graceful backend selection

The sandbox SHALL select an isolation backend by preference order podman -> docker -> bwrap -> none, MUST fall back to the next when a runtime is unavailable, and MUST fall back to `none` (run on the host) rather than failing to launch when no backend exists.

#### Scenario: Preferred runtime missing

- **WHEN** podman is not installed but docker is
- **THEN** the worktree process launches under docker

#### Scenario: No runtime available

- **WHEN** none of podman, docker, or bwrap is available
- **THEN** the process runs with backend `none` on the host and the worktree is
  still usable

### Requirement: Worktree stays on the host and is bind-mounted

A sandboxed worktree SHALL remain on the host filesystem and MUST be bind-mounted into the container at its real host path, so host-side git reads and the compositor continue to operate on the same files.

#### Scenario: Host git reads remain coherent

- **WHEN** a worktree process runs inside a container backend
- **THEN** the worktree is bind-mounted at its real path and git status/diff read
  from the host see the same working tree the sandboxed process edits

### Requirement: Sandboxing is per-worktree

Isolation SHALL be configurable per worktree and MUST NOT be a single global setting for the whole session.

#### Scenario: Mixed backends across worktrees

- **WHEN** two worktrees are open with different sandbox settings
- **THEN** each worktree's interactive process uses its own configured backend
  independently
