# Sidebar

## ADDED Requirements

### Requirement: Worktrees can be filed into folders

A workspace's worktrees MAY be filed into named folders. A folder SHALL
belong to exactly one workspace, render as a collapsible header with its
worktrees nested beneath it, and persist across restarts. Filing a worktree
into a folder MUST NOT move it on disk or change its git state. Deleting a
folder SHALL return its worktrees to the workspace root rather than deleting
them. A worktree whose recorded folder has no header in its workspace MUST
still render at the workspace root, never vanish from the tree.

#### Scenario: A filed worktree nests under its folder

- **WHEN** the user files a worktree into a folder
- **THEN** it renders beneath that folder's header and stays there across a
  restart, with its checkout and branch untouched

#### Scenario: Deleting a folder keeps its worktrees

- **WHEN** the user deletes a folder that contains worktrees
- **THEN** the folder disappears and its worktrees reappear at the
  workspace root

### Requirement: Manual ordering is scoped to a sibling run

Within a workspace, a worktree's ordering neighbourhood SHALL be its
**run**: the loose list of unfiled worktrees, or the folder it is filed
into. A manual reorder MUST move a worktree only among its own run's
members, and MUST NOT change the order of any other run. `home` SHALL be
anchored at the head of the loose run: it never moves and nothing may be
placed above it.

Moving a worktree past the head or tail of its run SHALL carry it into the
adjacent run — landing at the end of the previous run, or the head of the
next — and MUST update its folder membership to match. A **collapsed**
folder MUST be stepped over rather than entered, so a reorder can never hide
a worktree inside a folder the user has closed.

Reordering MUST work for workspaces that are not currently loaded into the
session.

#### Scenario: Reordering inside a folder leaves other runs alone

- **WHEN** the user reorders two worktrees filed into the same folder
- **THEN** their order within that folder changes and the loose list and
  every other folder keep their order

#### Scenario: Crossing a run edge re-files the worktree

- **WHEN** the user moves the first worktree of a folder upwards
- **THEN** it leaves that folder, lands at the end of the run above it, and
  its folder membership is updated to match

#### Scenario: A collapsed folder is stepped over

- **WHEN** the user moves a loose worktree down past a collapsed folder
- **THEN** the worktree skips that folder's contents entirely rather than
  being filed into a folder it cannot see

#### Scenario: A dormant workspace still reorders

- **WHEN** the user reorders worktrees in a workspace that is not loaded
  into the session
- **THEN** the new order applies and persists

### Requirement: Folders are manually ordered

Folders SHALL carry an explicit persisted position within their workspace
and be reorderable both by keyboard (with the cursor on the folder header)
and by dragging the header. A folder's worktrees MUST travel with it, so
reordering folders never changes any worktree's position or membership.

#### Scenario: Reordering a folder carries its worktrees

- **WHEN** the user moves a folder header up one slot
- **THEN** the folder and its nested worktrees move together, and the order
  of worktrees inside it is unchanged

### Requirement: A reorder persists the exact on-screen order

A manual reorder SHALL persist the workspace's whole resulting sequence
(`position = index`) rather than exchanging two positions, so a reload
reproduces exactly what the tree was showing. A single reorder MUST be
applied atomically: it can never leave a partially-reordered state.

#### Scenario: Reload reproduces the tree

- **WHEN** the user reorders worktrees and the workspace is re-read from
  the database
- **THEN** the restored order equals the order that was on screen, even if
  the rows started with absent or tied positions

## MODIFIED Requirements

### Requirement: Worktrees default to stable creation order

Within each **run** of a workspace (the loose list, and each folder), the
underlying manual arrangement SHALL be a stable creation-order sequence with
explicit, persisted manual reordering. The default _display_ sort is
Attention (see the attention-sort requirement); when no attention signals
distinguish worktrees — or before the first hydration pass — the displayed
order MUST equal this manual arrangement, so the list never reshuffles
without a real state change. A manual move made while a computed sort is
active MUST switch the workspace back to manual ordering so the move is
visible and survives a restart.

#### Scenario: Default order without signals is creation order

- **WHEN** worktrees are listed with no attention signals and no manual
  reordering
- **THEN** they appear in stable creation order

#### Scenario: Manual worktree reorder persists

- **WHEN** the user reorders worktrees
- **THEN** the new order persists across restarts

#### Scenario: A manual move under a computed sort switches to manual

- **WHEN** the user reorders a worktree while a computed sort is active
- **THEN** the workspace switches to manual ordering and the move is visible
  and persisted

### Requirement: Full mouse support with keyboard parity

The sidebar SHALL support: left-click select+activate (caret cell folds,
Ctrl-click marks), double-click that commits keyboard focus to the center
(or folds a header), right-click opening the row's context menu (which then
owns clicks and wheel), wheel navigation, and press-drag-release to reorder
worktrees within their run, folders among their workspace's folders, or
workspaces among themselves — with drops onto a folder filing the worktree
and onto its workspace header unfiling it. A drop resolved _between_ two
rows SHALL land the worktree in the run those rows belong to, filing it if
that differs from its current run, and MUST NOT spill past the end of that
run into the next one. Drag feedback (source lift, insertion rule, target
highlight) MUST derive from the same layout pass the renderer paints. Drops
MUST reuse the keyboard reorder/file machinery (persisted positions,
computed-sort→Manual flip, home anchoring; cross-workspace drops are
invalid). Mouse reporting MUST be enabled only when the outer terminal
supports it, and every mouse gesture MUST have a keyboard equivalent.

#### Scenario: Right-click opens the menu at the row

- **WHEN** the user right-clicks a worktree row
- **THEN** the cursor moves there and its context menu opens anchored under
  the row; clicking an entry runs it, clicking outside dismisses

#### Scenario: Drag files a worktree into a folder

- **WHEN** the user drags a worktree row onto a folder header of the same
  workspace and releases
- **THEN** the worktree files into that folder immediately (optimistic),
  with the durable write deferred

#### Scenario: Dropping inside a folder files and positions in one move

- **WHEN** the user releases a dragged worktree between two worktrees that
  are filed into a folder
- **THEN** the worktree is filed into that folder and placed at exactly the
  spot the insertion rule showed

#### Scenario: A drag can reorder folders

- **WHEN** the user drags a folder header above another folder in the same
  workspace and releases
- **THEN** the folders swap order and each folder's worktrees stay with it

#### Scenario: Drags never cross workspaces

- **WHEN** a worktree row is dragged over another workspace's subtree
- **THEN** the affordance shows an invalid drop and releasing changes nothing

#### Scenario: No mouse escapes on dumb terminals

- **WHEN** the host starts on a terminal without mouse support (e.g.
  `TERM=linux`)
- **THEN** no mouse-reporting escape sequences are emitted and the keyboard
  surface is unaffected
