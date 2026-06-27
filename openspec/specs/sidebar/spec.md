# Sidebar

## Purpose

The sidebar is the in-process navigation tree for the session: it shows
workspaces (repos) and their worktrees (tabs), reflects per-row activity, and
supports keyboard-driven navigation and manual ordering. Selecting an item is a
tab switch within the single session, never a session teleport.

## Requirements

### Requirement: Workspace/worktree tree model

The sidebar SHALL render workspaces and, under each, their worktrees from a host-side tree model, and selecting a worktree MUST switch to its tab within the one running session rather than spawning or teleporting to another session.

#### Scenario: Selecting a worktree switches tabs

- **WHEN** the user selects a worktree row
- **THEN** the session switches to that worktree's tab without spawning or
  teleporting to a separate session

### Requirement: Manual ordering independent of recency

Workspace ordering in the sidebar SHALL be controlled by an explicit persisted position and MUST NOT be reordered implicitly by last-active time.

#### Scenario: Reorder persists

- **WHEN** the user reorders a workspace via the reorder keybinding
- **THEN** the new position is persisted and the sidebar order is preserved across
  restarts, independent of which workspace was most recently active

### Requirement: Per-row activity indication

The sidebar SHALL surface per-row activity (e.g. activity dots) driven by the host-side activity state machine.

#### Scenario: Background activity shows on its row

- **WHEN** a non-focused worktree produces activity
- **THEN** its sidebar row reflects that activity state

### Requirement: Worktrees nest their tabs (pages) in the tree

A worktree MAY own multiple tabs (pages); the sidebar SHALL nest page rows under their worktree and MUST show them only when a worktree has more than one tab, with the main checkout presented as an explicit `home` worktree row that is a sibling of the branch worktrees.

#### Scenario: Single-tab worktree shows no page rows

- **WHEN** a worktree has exactly one tab
- **THEN** no page child rows are shown under it

#### Scenario: Multi-tab worktree shows page rows

- **WHEN** a worktree has more than one tab
- **THEN** its pages appear as child rows nested under that worktree

### Requirement: Worktrees default to stable creation order

Within a workspace, worktrees SHALL default to a stable creation-order arrangement and MUST support explicit manual reordering that persists, so the worktree list never reshuffles implicitly by activity.

#### Scenario: Default order is creation order

- **WHEN** worktrees are listed without any manual reordering
- **THEN** they appear in stable creation order

#### Scenario: Manual worktree reorder persists

- **WHEN** the user reorders worktrees
- **THEN** the new order persists across restarts
