# Sidebar

## ADDED Requirements

### Requirement: A unified chooser guards close and delete

The sidebar's delete key (`d` / `Delete`) SHALL open a row-kind-aware
disambiguation modal, never act directly: for worktrees, Close (keep branch
and files, the pre-selected default) versus Delete-from-disk (danger arm)
versus Cancel. When any target has uncommitted changes the modal MUST name
the dirty worktrees, pre-select Cancel, and open regardless of any
confirmation config. Removing a workspace MUST pre-select the keep-files arm,
and an unprompted workspace removal MUST NOT delete files from disk. Deleting
a folder MUST move its worktrees back to the workspace root without touching
disk.

#### Scenario: d on a clean worktree defaults to the safe close

- **WHEN** the user presses `d` on a clean worktree row and hits Enter
- **THEN** the worktree closes with its branch and files intact

#### Scenario: Dirty targets pre-select Cancel

- **WHEN** `d` targets a worktree with uncommitted changes
- **THEN** the modal names it, warns the work would be lost, and Enter alone
  cancels

#### Scenario: Unprompted workspace removal keeps files

- **WHEN** `confirm_delete_workspace = false` and the user removes a workspace
- **THEN** the workspace is forgotten but every worktree directory stays on
  disk

### Requirement: The context menu is the canonical action catalog

Every sidebar action SHALL be reachable from the row context menu (`m` or
right-click), grouped per row kind, with each entry showing the keyboard
shortcut that fires it directly and destructive entries rendered as danger.
The menu and the keyboard MUST dispatch through the same outcome path.

#### Scenario: The menu teaches the keys

- **WHEN** the user opens a worktree row's menu
- **THEN** entries like "Rename…" and "Move to folder…" display their key
  chips (`r`, `f`) and "Delete branch + files…" renders as danger

#### Scenario: Folder rows have folder actions

- **WHEN** the user opens a folder row's menu
- **THEN** rename, new-worktree-here and delete-folder (keeps worktrees) are
  offered

### Requirement: Creation and organization are reachable from the sidebar

While the sidebar owns focus, single keys SHALL cover the creation and
organization surface: `n` new worktree in the cursor row's workspace (new
terminal in the terminals region, also the empty hint's Enter action), `N`
new workspace, `b` a new worktree branched from the cursor row's branch,
`r`/F2 rename (worktree branch or folder name), `f` move-to-folder for a
worktree (new-folder on a workspace/folder row), and `c` copy path. The `s`
key SHALL open an explicit sort-mode menu (current mode indicated) rather
than blind-cycling.

#### Scenario: n creates where the cursor points

- **WHEN** the cursor rests on another workspace's worktree and the user
  presses `n`
- **THEN** the new-worktree wizard opens rooted at that workspace's repo

#### Scenario: F2 renames like an explorer

- **WHEN** the user presses F2 on a non-home worktree row
- **THEN** the rename prompt opens seeded with the current branch name

### Requirement: The sidebar documents itself

A `?` key SHALL show a grouped cheatsheet of the sidebar's key surface
(dismissed by any key), and while the sidebar owns focus the statusbar SHALL
lead with a curated handful of essential hints.

#### Scenario: Help is one key away

- **WHEN** the user presses `?` in the sidebar
- **THEN** a card lists the navigate/create/organize/act/view keys, and any
  key dismisses it

### Requirement: Full mouse support with keyboard parity

The sidebar SHALL support: left-click select+activate (caret cell folds,
Ctrl-click marks), double-click that commits keyboard focus to the center
(or folds a header), right-click opening the row's context menu (which then
owns clicks and wheel), wheel navigation, and press-drag-release to reorder
worktrees within their workspace or workspaces among themselves, with drops
onto a folder filing the worktree and onto its workspace header unfiling it.
Drag feedback (source lift, insertion rule, target highlight) MUST derive
from the same layout pass the renderer paints. Drops MUST reuse the keyboard
reorder/file machinery (persisted positions, computed-sort→Manual flip, home
anchoring; cross-workspace drops are invalid). Mouse reporting MUST be
enabled only when the outer terminal supports it, and every mouse gesture
MUST have a keyboard equivalent.

#### Scenario: Right-click opens the menu at the row

- **WHEN** the user right-clicks a worktree row
- **THEN** the cursor moves there and its context menu opens anchored under
  the row; clicking an entry runs it, clicking outside dismisses

#### Scenario: Drag files a worktree into a folder

- **WHEN** the user drags a worktree row onto a folder header of the same
  workspace and releases
- **THEN** the worktree files into that folder immediately (optimistic),
  with the durable write deferred

#### Scenario: Drags never cross workspaces

- **WHEN** a worktree row is dragged over another workspace's subtree
- **THEN** the affordance shows an invalid drop and releasing changes nothing

#### Scenario: No mouse escapes on dumb terminals

- **WHEN** the host starts on a terminal without mouse support (e.g.
  `TERM=linux`)
- **THEN** no mouse-reporting escape sequences are emitted and the keyboard
  surface is unaffected
