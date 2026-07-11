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

Workspace ordering in the sidebar SHALL default to an explicit persisted
position and MUST NOT be reordered implicitly by last-active time. A config
key (`[ui] sidebar_workspace_sort = "attention"`) MAY opt workspaces into
attention bubbling: a stable, tier-granular sort by each workspace's most
urgent worktree in which equal-tier workspaces MUST keep their manual order.

#### Scenario: Reorder persists

- **WHEN** the user reorders a workspace via the reorder keybinding
- **THEN** the new position is persisted and the sidebar order is preserved across
  restarts, independent of which workspace was most recently active

#### Scenario: Opt-in bubbling floats the urgent workspace

- **WHEN** `sidebar_workspace_sort = "attention"` and one workspace's worktree
  becomes blocked on the user
- **THEN** that workspace moves above equal-or-less-urgent workspaces while the
  rest keep their manual order

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

Within a workspace, the underlying manual arrangement SHALL be a stable
creation-order sequence with explicit, persisted manual reordering. The
default _display_ sort is Attention (see the attention-sort requirement);
when no attention signals distinguish worktrees — or before the first
hydration pass — the displayed order MUST equal this manual arrangement, so
the list never reshuffles without a real state change.

#### Scenario: Default order without signals is creation order

- **WHEN** worktrees are listed with no attention signals and no manual
  reordering
- **THEN** they appear in stable creation order

#### Scenario: Manual worktree reorder persists

- **WHEN** the user reorders worktrees
- **THEN** the new order persists across restarts

### Requirement: Worktrees carry a tiered attention score

Every worktree SHALL carry an attention score derived off-loop from existing
signals (activity state machine, unread notifications, PR/CI caches, merge
queue), tiered most-urgent-first: blocked-on-user, failure, finished-awaiting
-user, ready-to-land, working, idle. A dirty working tree MUST NOT raise a
tier on its own. Ties within a tier SHALL order longest-waiting first using
real event timestamps where the source has them.

#### Scenario: Blocked outranks failure outranks finished

- **WHEN** one worktree has an unread agent-attention notification, another a
  failing CI check, and a third an idle-after-activity (waiting) dot
- **THEN** the attention order is blocked, then failure, then waiting

#### Scenario: Dirty alone stays idle

- **WHEN** a worktree's only signal is uncommitted changes
- **THEN** its tier is idle (dirty only sub-ranks within idle)

### Requirement: Attention sort is the default and is churn-stable

The sidebar SHALL provide an Attention sort mode that orders worktrees within
a workspace by their attention rank, and it SHALL be the default for sessions
without a persisted sort mode. The persisted legacy value `activity` MUST
parse as Attention. Ordering MUST be hysteresis-stable: rows reorder only on
a tier or membership change, never from cache refreshes or timestamp ticks;
before the first hydration pass the mode MUST degrade to the manual order.

#### Scenario: Saved activity mode migrates

- **WHEN** a session's persisted sort mode is the legacy string `activity`
- **THEN** it loads as the Attention sort mode

#### Scenario: Cache churn does not reshuffle

- **WHEN** the PR cache refreshes with no underlying state change
- **THEN** the displayed worktree order is unchanged

#### Scenario: Manual move under attention sort flips to manual

- **WHEN** the user manually reorders a worktree while Attention sort is active
- **THEN** the sort mode flips to Manual so the move is visible and persists

### Requirement: One-key jump to the next worktree needing the user

A bindable action (`attention-next`, default `Alt a`) SHALL focus the most
urgent worktree needing the user (tiers blocked/failure/waiting), cycling
through that set on repeat with wrap-around. It MUST work regardless of the
active sort mode and MUST cross workspaces (switching workspace when the
target lives in a dormant one). When nothing needs the user it MUST be a
harmless no-op with a status message.

#### Scenario: Jump cycles the needs-you set

- **WHEN** the user presses the jump key repeatedly with three worktrees
  needing attention
- **THEN** focus visits them most-urgent-first and wraps back to the first

### Requirement: Statusbar needs-you chip with drill-down

The statusbar SHALL show a chip counting worktrees that need the user, colored
red while any is blocked or failing and amber when only finished work waits,
and silent at zero. Activating the chip MUST open a detail list (worktree —
reason — age) whose rows focus their worktree on Enter.

#### Scenario: Chip counts and colors

- **WHEN** two worktrees wait for review and one agent is blocked on input
- **THEN** the chip shows 3 in red
