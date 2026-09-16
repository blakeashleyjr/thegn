# Sidebar

## Purpose

The sidebar is the in-process navigation tree for the session: it shows
workspaces (repos) and their worktrees (tabs), reflects per-row activity, and
supports keyboard-driven navigation and manual ordering. Selecting an item is a
tab switch within the single session, never a session teleport.

## Requirements

### Requirement: Workspace/worktree tree model

The sidebar SHALL render workspaces and, under each, their worktrees from a
host-side tree model, and selecting a worktree MUST switch to its tab within
the one running session rather than spawning or teleporting to another
session. Tabs (pages) MUST NOT appear in the sidebar — they live in the
tabbar. A dormant workspace's subtree SHALL be structurally identical to its
live rendering: same folders, sort order, pin behavior, and gutter alignment.

#### Scenario: Selecting a worktree switches tabs

- **WHEN** the user selects a worktree row
- **THEN** the session switches to that worktree's tab without spawning or
  teleporting to a separate session

#### Scenario: Tabs never render in the tree

- **WHEN** a worktree owns multiple tabs
- **THEN** no page child rows appear under it (tab switching lives in the
  tabbar)

#### Scenario: Dormant and live trees match

- **WHEN** the same workspace renders live and then dormant (parked)
- **THEN** the tree shape — folders, order, pinned rows — is identical

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

The sidebar SHALL surface per-row activity (e.g. activity dots) driven by the
host-side activity state machine.

The indication SHALL distinguish four things: that a worktree is **working**;
that its agent has **finished** and awaits the user; that its agent is
**blocked on the user**; and whether the user has **seen** an awaiting state.
Working, finished, and blocked MUST be visually distinct from one another, and
seen-versus-unread MUST be carried on a separate axis from that distinction so
the two read independently.

Whether an awaiting worktree is blocked rather than merely finished SHALL be
taken from its attention tier, so the loud state is reserved for cases with real
evidence behind them (an agent asking for input, a queue needing a human) rather
than inferred from output.

A worktree with no agent MUST NOT show any awaiting state (see the
`activity-signals` capability).

#### Scenario: Background activity shows on its row

- **WHEN** a non-focused worktree produces activity
- **THEN** its sidebar row reflects that activity state

#### Scenario: Finished and blocked read differently

- **WHEN** one worktree's agent has finished and another's is blocked on the
  user
- **THEN** their rows show visually distinct awaiting indications

#### Scenario: Seen but still waiting

- **WHEN** the user focuses a tab whose worktree is awaiting them
- **THEN** the row's indication changes to the seen form while keeping the same
  finished-versus-blocked distinction, and is not cleared

#### Scenario: A plain terminal never demands attention

- **WHEN** a worktree with no agent goes busy and then quiet
- **THEN** its row shows the working indication and then none, never an awaiting
  one

### Requirement: Worktrees default to stable creation order

Within a workspace, the manual arrangement SHALL be a stable creation-order
sequence with explicit, persisted manual reordering — and Manual SHALL be
the default display sort: the tree never reorders itself unless the user
picks a computed sort. Urgency still surfaces at the default through
activity dots, the statusbar needs-you chip, and the attention jump key.

#### Scenario: Default order without signals is creation order

- **WHEN** worktrees are listed with no attention signals and no manual
  reordering
- **THEN** they appear in stable creation order

#### Scenario: Attention signals alone do not reorder the default

- **WHEN** the sort mode is the (default) Manual and a worktree becomes
  blocked on the user
- **THEN** the displayed order is unchanged (the row's dot/chip reflect the
  urgency instead)

#### Scenario: Manual worktree reorder persists

- **WHEN** the user reorders worktrees
- **THEN** the new order persists across restarts

#### Scenario: A manual move under a computed sort switches to manual

- **WHEN** the user reorders a worktree while a computed sort is active
- **THEN** the workspace switches to manual ordering and the move is visible
  and persisted

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

### Requirement: Configurable, resizable sidebar width

The sidebar's width SHALL be adjustable at runtime and persisted, by three
equivalent routes writing one stored width: the zone keys `<` / `>` (aliased
`,` / `.`) while the sidebar has focus, the bindable actions
`sidebar-narrower` / `sidebar-wider` (defaults `Ctrl Alt ,` / `Ctrl Alt .`)
from any zone, and dragging the sidebar's separator with the mouse.
`[ui] sidebar_width` SHALL set the resting width a fresh install starts at,
and `[ui] sidebar_wide_ratio` the fraction of the window the wide expand
(`e`) claims; a stored runtime width MUST take precedence over the config
key. Every route MUST clamp to a floor of 12 columns and a ceiling of ~half
the window, and MUST report the width it settled on so a nudge that reaches
the clamp is distinguishable from a dead key.

The drag's grab target SHALL be a two-column band — the separator column plus
the adjacent pane frame cell, the second vertical rule at that boundary —
with the extra cell skipped whenever it is pane or drawer content
(`hit_pane`); the separator column itself always grabs. The divider SHALL
hold its grab offset for the whole drag, so it stays under the cursor instead
of jumping to it on the first sample. A press that never moves MUST change
nothing: no width change, no drop out of the wide expand, no persist, and no
width report — a bare click on the divider is a no-op.

Width applies to the **full** tree only. In rail mode the nudge and the drag
MUST be refused with a pointer rather than persisting a width that
`effective_cols` ignores. Setting a width while the wide expand is active
MUST drop out of the expand so the requested width takes effect; for the
drag, the drop-out happens when the gesture becomes real (the first pointer
sample that moved), not on the press. `Esc` while a width drag is in flight
MUST cancel it, restoring the pre-drag width and persisting nothing; Esc
never half-applies.

#### Scenario: Nudge, drag, and config agree on one width

- **WHEN** the user drags the separator, then restarts
- **THEN** the sidebar returns at the dragged width, and `<` / `>` continue
  from it rather than from `[ui] sidebar_width`

#### Scenario: The ceiling follows the window

- **WHEN** the user widens the sidebar to the clamp on a 200-column window
- **THEN** it stops at 100 columns, and the status line names that width

#### Scenario: Rail refuses a resize

- **WHEN** the user presses `>` or drags the separator while the sidebar is
  in rail mode
- **THEN** no width is stored and the status line points at the key that
  grows the rail back

#### Scenario: The band grabs the divider or the frame cell beside it

- **WHEN** the user presses on the pane frame cell adjacent to the separator
  while that cell is not pane or drawer content
- **THEN** the width drag grabs as if the divider itself had been pressed

#### Scenario: The extra cell yields to pane and drawer content

- **WHEN** the user presses on the band's extra cell in a row where the
  bottom drawer's content occupies it
- **THEN** the press reaches the drawer (or pane), not the width drag; only
  the separator column itself grabs there

#### Scenario: A click on the divider changes nothing

- **WHEN** the user presses the separator and releases without moving the
  pointer while the sidebar is wide-expanded
- **THEN** the sidebar stays expanded, no width is stored, and the status
  line does not report a width

#### Scenario: The divider keeps its grab offset

- **WHEN** the user presses one column off the separator and then drags
- **THEN** the separator trails that one-column offset for the whole drag
  instead of jumping to the pointer

#### Scenario: Esc cancels a width drag

- **WHEN** the user presses `Esc` after dragging the separator to a new width
- **THEN** the sidebar returns to the width it had at the press and nothing
  is persisted

### Requirement: The scroll window reaches the end of the list

The sidebar's scroll offset SHALL be state independent of the cursor, clamped
only to `[0, max_scroll]` where `max_scroll` is the smallest offset whose
remaining rows fit the viewport. Every row MUST therefore be reachable,
including the last. Moving the cursor MUST NOT be the only way to move the
window, and a rebuild that repositions the cursor while the sidebar is
unfocused MUST NOT reposition the window.

#### Scenario: The bottom row is reachable

- **WHEN** the tree is taller than the sidebar viewport and the user scrolls to
  the end
- **THEN** the last row is laid out in full, not clipped or omitted

#### Scenario: An unfocused rebuild leaves the window alone

- **WHEN** the sidebar is scrolled away from the active worktree, is unfocused,
  and a hydration tick or git-watch event rebuilds the rows
- **THEN** the scroll offset is unchanged and the same rows stay on screen

#### Scenario: The wheel scrolls the viewport, not the selection

- **WHEN** the user scrolls the wheel over the sidebar
- **THEN** the window moves and the cursor stays on its row; a subsequent
  cursor-relative key first re-anchors the cursor into the visible window so no
  action can target an off-screen row

### Requirement: Truncation is never silent

WHEN the sidebar viewport cannot show every row, it SHALL indicate this — a
scroll position affordance plus a count of the rows hidden in each direction.
Rows MUST NOT be dropped from the bottom (or top) of the list without a visible
signal, because a clipped row is otherwise indistinguishable from a deleted
workspace.

#### Scenario: Hidden rows are counted

- **WHEN** the tree is taller than the viewport
- **THEN** the sidebar shows how many rows are hidden below (and above, once
  scrolled), and the indication disappears in a direction as soon as nothing is
  hidden that way

#### Scenario: The affordances are not click targets

- **WHEN** the user clicks the scroll affordance or the hidden-row count
- **THEN** the click resolves to the row beneath it, exactly as if the
  affordance were not painted

### Requirement: A live agent pipeline shows as one compact sidebar row

While the agent-dispatch roster holds live rows, the sidebar SHALL show a single
compact rollup row naming the pipeline and the number of live dispatches, with
the human-parked count shown separately in the attention tone when it is
non-zero. The row SHALL count roster ROWS, not worktrees, so a fan-out of chunk
rows inside one worktree is not under-reported.

The row SHALL NOT appear when the roster has no live rows: it is evidence of
running work, not permanent chrome.

The row is a door, not a destination: it SHALL NOT be a navigation target, a
collapsible header, or a member of the multi-select set, and `↵` or a click on it
SHALL run the pipeline-board action through the same seam both input paths
already share, so keyboard and mouse cannot diverge.

Its data SHALL come from the existing off-loop hydration pass — the same roster
read that already produces the per-worktree stage tags — introducing no new query
and no new wake source, and a roster change SHALL repaint the sidebar without
forcing a full chrome recompose.

The row SHALL be placed at the tail of the tree, after every workspace row,
because it appears and disappears with the roster and the sidebar cursor is a
visible-row index: a placement above the workspace rows would move the cursor off
the row under it every time an agent started or finished.

#### Scenario: Running agents earn a row

- **WHEN** the roster holds three live dispatches, one of them parked on a human
- **THEN** the sidebar shows one pipeline row reporting three live and one
  waiting, the waiting count in the attention tone

#### Scenario: An idle roster shows nothing

- **WHEN** the roster holds no live rows
- **THEN** no pipeline row is present in the tree

#### Scenario: The row opens the board

- **WHEN** the user presses `↵` on the pipeline row, or clicks it
- **THEN** the pipeline-board action runs

#### Scenario: The row does not move the cursor's neighbours

- **WHEN** the roster gains its first live row while the cursor rests on a
  worktree
- **THEN** the pipeline row appears below every workspace row and the cursor
  still rests on the same worktree

### Requirement: Submodule state has a distinct optional indicator

The sidebar SHALL be able to render a separate submodule-dirty/conflicted
indicator from the cached Git read model, controlled by its `[ui]` visibility
setting. Disabling that indicator MUST NOT change Git state or other dirty
signals.

#### Scenario: Indicator is hidden by preference

- **WHEN** the submodule-status visibility setting is disabled
- **THEN** the submodule glyph is omitted while all underlying state and other
  sidebar indicators remain unchanged

### Requirement: Header rows read in tiers

The full sidebar SHALL render its structural rows in visually distinct tiers:
workspace (and terminal-host) headers as the strongest tier, folder headers
as a clearly secondary tier, and worktree/terminal rows as the body tier. A
workspace header and a folder header MUST be distinguishable at a glance by
more than indentation alone, and the distinction MUST NOT rely on color
alone — it survives 16-color and mono quantization through weight and
layout. All styling MUST resolve through the theme slot / capability-glyph
chokepoints; no color or glyph literal at a draw site.

#### Scenario: A repo and its folder are told apart

- **WHEN** a workspace containing a "Merged" folder is rendered in the full
  sidebar
- **THEN** the workspace header and the folder header use visibly different
  emphasis (not merely different indent), with the folder subordinate

#### Scenario: The hierarchy survives a mono terminal

- **WHEN** the same tree renders with colors quantized to mono
- **THEN** workspace headers, folder headers and worktree rows remain
  distinguishable by weight and layout

### Requirement: Adjacent projects are visibly separated

The sidebar SHALL separate one project's subtree from the next by an
alternating background tint, gated by `[ui] sidebar_dividers` (default on).
Each project block — its header row and every row beneath it up to the next
block head — SHALL share one tint, and consecutive blocks SHALL alternate
between the `panel` and `panel_alt` palette slots; a section banner SHALL
reset the alternation so a following region always opens on the base tint.
Project and terminal-host headers SHALL keep their recessed `bg0` band on
both parities, and `panel_alt` SHALL be derived to sit between `bg0` and
`panel` and never past their midpoint, so a header still reads as the start
of its block whichever tint the block took.

Because the separation costs no layout rows, it SHALL apply in rail mode and
while the `/` filter is active, and row geometry SHALL be identical with
`sidebar_dividers` on and off. With `sidebar_dividers = false` every block
SHALL render on the base tint.

The separation MUST NOT be a blank separator row: an earlier form of this
requirement spent one screen row per project, which on a tree of a dozen or
more repos consumed a large fraction of the column it was meant to make
legible.

#### Scenario: Two repos no longer read as one

- **WHEN** two projects render consecutively with `sidebar_dividers = true`
- **THEN** the second project's rows carry a different background tint from
  the first's, and no blank row lies between them

#### Scenario: A project block is tinted as a unit

- **WHEN** a project block contains worktree rows, folder headers and a
  derived `Pipelines` folder
- **THEN** every one of those rows carries the same tint as its project
  header's block, so the block reads as one thing

#### Scenario: The header band survives both parities

- **WHEN** a project header renders on an alternate-tinted block
- **THEN** it keeps the `bg0` band, which stays at least as distinct from the
  block tint as the two block tints are from each other

#### Scenario: Separation costs no rows

- **WHEN** the same tree is laid out with `sidebar_dividers` on and off
- **THEN** the two layouts have identical row heights and scroll geometry,
  differing only in the background tint of alternate blocks

#### Scenario: The tint survives the rail and a filter

- **WHEN** the sidebar is in rail mode, or a `/` filter is active
- **THEN** consecutive project blocks still alternate tint

#### Scenario: Alternation can be turned off

- **WHEN** `[ui] sidebar_dividers = false`
- **THEN** every project block renders on the base `panel` tint

### Requirement: Pipeline rows only ever nest under a project

The sidebar SHALL render derived pipeline folders only inside the project that
owns their worktrees. No pipeline-shaped row — group, lane, worktree mirror, or
roster rollup — SHALL be emitted at the root of the tree.

A lane's project SHALL be resolved from the first of its worktrees that
resolves through the live session groups or the DB-registered worktrees; when
none does, it SHALL be resolved from the directory holding that project's other
worktrees, and a directory claimed by more than one project SHALL be treated as
ambiguous and ignored. A lane that resolves to no project SHALL contribute no
rows to the tree; it remains on the pipeline board, which is the complete view
of the roster.

The flat layout has no project rows to nest under and SHALL therefore emit no
pipeline rows.

The pipeline board SHALL remain reachable independently of any sidebar row, via
`Action::OpenPipelineBoard`.

#### Scenario: A lane nests under the project owning its worktrees

- **WHEN** a lane's worktree resolves to a registered project
- **THEN** its `Pipelines` group, lane folder and worktree mirrors render
  inside that project's subtree, and none of them at depth 0

#### Scenario: An unregistered sibling worktree still finds its project

- **WHEN** a lane's only worktree is not in the session or the database, but
  sits in the directory holding that project's registered worktrees
- **THEN** the lane files under that project

#### Scenario: An unattributable lane is left out of the tree

- **WHEN** no worktree of a lane resolves to any project, directly or by
  sibling directory
- **THEN** the tree contains no group, lane or mirror row for it, and no
  top-level `Pipelines` group is created to hold it

#### Scenario: The flat layout grows no pipeline rows

- **WHEN** the sidebar is in the flat layout and the roster has lanes
- **THEN** no pipeline group, lane or mirror row is emitted

#### Scenario: The board is reachable without a sidebar row

- **WHEN** the sidebar renders no pipeline rows at all
- **THEN** `Action::OpenPipelineBoard` still opens the board

### Requirement: The row-drag drop target covers the sidebar's rect

Dragging a row to reorder it SHALL resolve the release against the full
visual extent of the sidebar's rows: a release anywhere inside the sidebar's
rect SHALL land on the nearest row, with the blank tail below the last row
landing at the end of the list, and a release outside the sidebar's rect
SHALL cancel the drag without moving anything. The drop target MUST NOT
shrink to the painted text of a row: the whole row line is live.

#### Scenario: The blank tail below the last row is a live drop zone

- **WHEN** the user drags a row and releases in the blank area below the
  last row, still inside the sidebar
- **THEN** the dragged row lands at the end of the list

#### Scenario: A release outside the sidebar cancels

- **WHEN** the user drags a row and releases outside the sidebar's rect
- **THEN** nothing moves, exactly as `Esc` would leave it

### Requirement: A mouse drag reorders sidebar rows by the row-slot rule

Releasing a sidebar drag over a row SHALL place the dragged item in the slot that
row occupied before the drop, and the displaced row SHALL shift one step toward
where the dragged item came from. The rule MUST NOT depend on where within a row
the pointer sits: a sidebar row can be one terminal cell tall, so a rule that
splits a row into halves is undefined for it.

Consequently every slot of a run MUST be reachable, including the last one:
hovering the final row of a run SHALL land the dragged item at that run's end.
The tail of a run the source does not already belong to SHALL remain reachable
through that run's header — a folder header files at the end of its folder, and a
workspace header unfiles at the end of the loose run.

The `home` row SHALL remain anchored at the head of its workspace's loose run: it
is never a drag source, and its slot is never a destination. A worktree drag
SHALL NOT cross workspaces.

A drop SHALL name its destination by a stable row identity rather than a resolved
index, and SHALL be abandoned when that row has vanished or moved to another run
mid-drag, rather than landing the item at a guessed slot.

#### Scenario: Dropping on a row takes that row's slot

- **WHEN** a run reads `[a, b, c, d]` and `a` is dragged onto `c`
- **THEN** the run reads `[b, c, a, d]`

#### Scenario: Dropping on the last row lands at the end

- **WHEN** a run reads `[a, b, c, d]` and `a` is dragged onto `d`
- **THEN** the run reads `[b, c, d, a]`

#### Scenario: Dropping from below takes the hovered row's slot

- **WHEN** a run reads `[a, b, c, d]` and `d` is dragged onto `b`
- **THEN** the run reads `[a, d, b, c]`

#### Scenario: The rule holds at every row height

- **WHEN** the same drop is made with the sidebar unfocused, focused, and under
  each `sidebar_focus_detail` setting — that is, with rows one or two cells tall
- **THEN** the resulting order is the same in every case

#### Scenario: A vanished anchor abandons the drop

- **WHEN** the row a drop is aimed at is deleted or re-filed before the release
- **THEN** nothing is reordered

### Requirement: A drag gesture holds the sidebar's geometry and the pointer

While a sidebar drag is armed or in flight, sidebar row heights SHALL stay as the
pressed frame painted them. Focus changes and cursor movement otherwise resize
rows through the focused-detail tier, which moves rows under a stationary pointer
and silently changes the drop target.

A live drag SHALL also capture the pointer: mouse events MUST NOT be forwarded to
or consumed by a mouse-reporting pane for the duration of the gesture, so the
release always reaches the sidebar and the gesture always ends. Pressing `Esc`
SHALL abandon an in-flight drag without reordering anything.

Edge autoscroll during a drag SHALL advance in proportion to how far past the
list edge the pointer is, so that a burst of motion samples coalesced into one
still travels the distance the pointer travelled.

#### Scenario: Focus arriving after the press does not move the rows

- **WHEN** a row is pressed while the sidebar is unfocused and focus then moves to
  the sidebar
- **THEN** every screen row still resolves to the row it resolved to at press time

#### Scenario: A release over a mouse-reporting pane still ends the drag

- **WHEN** a drag's pointer crosses a pane whose application requested mouse
  reporting, and the button is released there
- **THEN** the gesture ends and does not affect any later drag

#### Scenario: Esc abandons a drag

- **WHEN** `Esc` is pressed during a drag
- **THEN** the gesture ends and no row has moved

### Requirement: A drop applies as a single resolved order

A mouse drop SHALL compute the complete new order once and apply it once, for
worktrees, folders and workspaces alike. A drop that is refused — because the
workspace order is computed by attention, or because a participant is pinned and
therefore floated by the renderer — SHALL leave the order exactly as it was.

#### Scenario: A refused workspace drop changes nothing

- **WHEN** a workspace drop is refused because a participating workspace is pinned
- **THEN** the on-screen and stored workspace order are unchanged

#### Scenario: A mouse drop persists what is on screen

- **WHEN** a worktree is dropped into a new position
- **THEN** reloading from the database reproduces the on-screen order exactly, and
  any change of folder membership survives with it

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

#### Scenario: Creation retains its folder context

- **WHEN** the user starts a worktree from a folder row or a worktree filed in
  that folder
- **THEN** completion files the registered worktree into the captured folder
  only while its repository, ID and name still match; a recoverable Halted
  creation retains this filing behavior, and terminal events consume only
  their own creation generation
- **AND** a changed or deleted destination leaves the worktree unfiled without
  creating a folder; a deferred write rechecks the destination and repository

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

Selecting a resident worktree by mouse SHALL invalidate the center contents
in the same frame as the sidebar selection. Switching to another workspace
SHALL retain the existing workspace switch behavior.

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

#### Scenario: Mouse selection repaints a quiet terminal

- **WHEN** two resident worktrees have different terminal contents and the user
  clicks the inactive worktree while both terminals are quiet
- **THEN** the first frame displaying the new selection also displays that
  worktree's terminal contents, without requiring further input or output

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

### Requirement: Persisted sidebar view state is tombstone-free and pruned

The sidebar's persisted view state SHALL live in the single global `ui_state`
scope `sidebar`, and boolean keys (`collapse:*`, `pin:*`) MUST be deleted —
never tombstoned with a `"0"` value — when they return to their default
state. Loading MUST sweep legacy tombstone rows and rewrite legacy sort-mode
spellings (`activity`) to the canonical value. Removing a workspace,
worktree, or folder MUST prune its `collapse:`/`pin:` keys by prefix so
`ui_state` never accumulates orphans.

#### Scenario: Unpinning deletes the key

- **WHEN** the user unpins a row
- **THEN** its `pin:` key is removed from `ui_state` rather than set to `"0"`

#### Scenario: Removing a workspace prunes its view keys

- **WHEN** a workspace is removed
- **THEN** every `collapse:`/`pin:` key under its slug (including folder and
  worktree variants) is deleted

### Requirement: Sidebar glyphs are capability-routed and degrade to ASCII

Every glyph the sidebar renders SHALL come from the capability-resolved glyph
table, and rendering under ASCII capabilities MUST produce pure 7-bit ASCII
output. Chrome glyphs MUST be Basic-Multilingual-Plane characters with
display width 1 (no astral-plane or emoji-presentation characters). The
merge-queue status vocabulary SHALL be a single shared mapping consumed by
every surface that renders it.

#### Scenario: ASCII terminal renders pure ASCII

- **WHEN** the sidebar renders under `Ascii` glyph capabilities with folders,
  terminals, badges and the detail line populated
- **THEN** every cell of the frame is 7-bit ASCII

#### Scenario: Sidebar and panel agree on merge-queue glyphs

- **WHEN** a branch's merge-queue status renders in the sidebar detail chip
  and the panel's queue section
- **THEN** both show the same glyph and hue for that status

### Requirement: The TERMINALS section visibility is configurable

The sidebar SHALL show the TERMINALS section banner by default even when no
terminals exist (with an actionable empty hint), and
`[ui] sidebar_terminals_section = "nonempty"` MUST hide the entire section
until a terminal exists.

#### Scenario: Empty section hides under nonempty

- **WHEN** `sidebar_terminals_section = "nonempty"` and no terminals exist
- **THEN** neither the TERMINALS banner nor its hint row renders

### Requirement: Rail mode preserves row-kind identity

The slim rail SHALL keep workspaces and terminals identifiable: workspace
rows show a bold initial, terminal rows show the activity-dot + initial
treatment worktrees get, and empty-hint rows render nothing.

#### Scenario: A terminal keeps its identity at rail width

- **WHEN** the sidebar is in rail mode with a terminal row
- **THEN** that row shows a dot cell and the terminal's first letter rather
  than a generic divider

### Requirement: Attention sort is available and churn-stable

The sidebar SHALL provide an Attention sort mode that orders worktrees within
a workspace by their attention rank. The persisted legacy value `activity`
MUST parse as Attention. Ordering MUST be hysteresis-stable: rows reorder
only on a tier or membership change, never from cache refreshes or timestamp
ticks; before the first hydration pass the mode MUST degrade to the manual
order.

#### Scenario: Saved activity mode migrates

- **WHEN** a session's persisted sort mode is the legacy string `activity`
- **THEN** it loads as the Attention sort mode

#### Scenario: Cache churn does not reshuffle

- **WHEN** the PR cache refreshes with no underlying state change
- **THEN** the displayed worktree order is unchanged

#### Scenario: Manual move under attention sort flips to manual

- **WHEN** the user manually reorders a worktree while Attention sort is active
- **THEN** the sort mode flips to Manual so the move is visible and persists
