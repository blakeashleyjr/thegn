# panes Specification

## Purpose

TBD - created by archiving change add-multiplexer-parity. Update Purpose after archive.

## Requirements

### Requirement: Pane splits are resizable from the keyboard

thegn SHALL provide keyboard resize actions (`resize-left`, `resize-right`,
`resize-up`, `resize-down`) that shift weight between the focused pane's
branch and its neighbor at the nearest ancestor split matching the direction,
by a fixed step, clamped so no pane's share reaches zero. A resize with no
matching-axis ancestor MUST be a harmless no-op with a statusbar hint. The
adjusted weights MUST persist through the existing tab-layout persistence and
survive resurrection.

#### Scenario: Growing the focused pane

- **WHEN** the focused pane sits left of a sibling in a row split and the user
  invokes `resize-right`
- **THEN** the focused pane's weight grows and the sibling's shrinks by the
  step, the frame repaints, and the new weights are persisted

#### Scenario: Resize cannot eliminate a pane

- **WHEN** the user repeats a resize action until the neighbor reaches the
  minimum share
- **THEN** further repeats leave the weights unchanged rather than collapsing
  the neighbor to zero cells

#### Scenario: No matching axis

- **WHEN** the focused tab holds a single pane and the user invokes a resize
  action
- **THEN** nothing changes and a statusbar hint explains there is nothing to
  resize

### Requirement: Panes swap positions from the keyboard

thegn SHALL provide swap actions (`swap-pane-left/right/up/down`) that
exchange the focused leaf with its spatial neighbor in that direction, where
the neighbor is resolved by the same geometry walk as the `Focus*` actions so
focus and swap always agree. The panes exchange tree positions (each adopts
the other slot's weight); focus follows the moved pane. Swapping toward an
edge with no neighbor MUST be a no-op.

#### Scenario: Swap with the right neighbor

- **WHEN** two panes sit side by side at 70/30 and the user invokes
  `swap-pane-right` from the left pane
- **THEN** the panes exchange positions, the moved pane now occupies the 30%
  slot, and focus stays on the moved pane

#### Scenario: Swap agrees with focus

- **WHEN** `focus-down` from a pane would land on pane B
- **THEN** `swap-pane-down` from that pane exchanges it with pane B, never a
  different pane

### Requirement: Pane borders drag-resize with the mouse

Pane frame borders SHALL act as mouse drag handles: pressing on a shared
border and dragging along the split axis shifts weight between the adjacent
branches continuously (same clamps as keyboard resize), committing one
persisted layout on release. Mouse events inside a pane's content rect MUST
remain governed by the existing pane mouse forwarding — chrome drag behavior
binds only to frame cells.

#### Scenario: Dragging a vertical border

- **WHEN** the user presses on the border between two side-by-side panes and
  drags right
- **THEN** the left pane widens live as the pointer moves and the final
  weights are persisted once on release

#### Scenario: Content clicks still reach the application

- **WHEN** a full-screen application has requested mouse reporting and the
  user clicks inside that pane's content
- **THEN** the click is forwarded to the application and no compositor drag
  begins

### Requirement: Panes rearrange by dragging their frames

Pressing and dragging on a pane's frame/title SHALL lift the pane and show a
live drop-target highlight as the pointer moves: hovering another pane's
center region targets a swap; hovering an edge band targets re-anchoring the
dragged pane as a new split on that side. Release commits the highlighted
operation through the same tree mutations as the keyboard; Esc or release
over a non-target cancels with no layout change. Drop-target resolution MUST
be a pure function of the pointer cell and the pane rects, and drag feedback
MUST render as chrome damage. Every drop outcome MUST also be reachable from
the keyboard (swap actions; splits + swaps compose to any re-anchor).

#### Scenario: Drop on a pane center swaps

- **WHEN** the user drags pane A's frame onto the center of pane B and
  releases
- **THEN** A and B exchange positions exactly as `swap-pane` would

#### Scenario: Drop on an edge re-anchors

- **WHEN** the user drags pane A onto the bottom edge band of pane B and
  releases
- **THEN** A is removed from its slot and B's slot becomes a column split with
  B above A

#### Scenario: Esc cancels a lift

- **WHEN** the user lifts a pane and presses Esc before releasing
- **THEN** the drag ends, the highlight clears, and the layout is unchanged

#### Scenario: Drag feedback never recomposes from pane output

- **WHEN** a lifted pane's process writes output mid-drag
- **THEN** the output renders through the normal pane damage path and the drag
  highlight repaints only on pointer motion or drag-state change

### Requirement: Zoom is remembered per tab

thegn SHALL keep the `Ctrl-Alt-z` zoom level (tiled, maximized, full-window)
on each worktree tab, and SHALL keep each tab's focused pane across tab,
worktree and workspace switches. Activating a tab MUST NOT move its focus
unless the remembered pane no longer exists. The chrome MUST follow the active
tab's level. A full-window tab whose focus moves to the sidebar or panel SHALL
show that chrome until focus returns to the center. The level is not
persisted: a restart comes back tiled.

#### Scenario: Returning to a zoomed worktree

- **WHEN** the user zooms the second pane of a split in worktree A, switches
  to worktree B, and switches back to A
- **THEN** A shows that same second pane zoomed, and B was shown with its own
  (unzoomed) layout

#### Scenario: A stale focus is repaired

- **WHEN** a tab is activated whose remembered focused pane is no longer in
  its tree
- **THEN** focus moves to the leftmost visible pane

#### Scenario: Keyboard navigation within a saved stack survives restoring tiled

- **WHEN** the user zooms a saved stack, uses Ctrl or Alt vertical navigation
  to focus a collapsed member, and restores tiled view
- **THEN** the original stack expands that same member and keyboard input
  targets the displayed pane, including stacks nested inside splits

### Requirement: A zoomed tab renders as a pane stack

While a tab is maximized or full-window, thegn SHALL draw the focused pane
expanded and every other pane of the tab as a one-row title bar: panes before
it in tree order above it, and panes after it below. A left click on a bar
SHALL focus and expand that pane. When the center is too short to keep a
usable expanded pane under the bars, the bars SHALL be omitted. Vertical focus
moves SHALL step through the stack in its visual order; horizontal moves keep
the split geometry.

#### Scenario: Stack bars show the hidden panes

- **WHEN** the user zooms the middle pane of a three-pane tab
- **THEN** one collapsed title bar is drawn above the expanded pane and one
  below it

#### Scenario: Clicking a bar

- **WHEN** the user clicks a collapsed bar while zoomed
- **THEN** that pane becomes focused and expanded, and the tab stays zoomed

#### Scenario: A stack is too narrow to paint its bars

- **WHEN** a stack is laid out in fewer than two columns
- **THEN** collapsed bars are omitted from both rendering and hit-testing and
  the active member keeps the stack's full rect

#### Scenario: A stack bar is not displayed

- **WHEN** an app tab or loading splash replaces the worktree center, or an
  overlay covers the cell where a collapsed bar would otherwise be drawn
- **THEN** a click there MUST NOT activate the hidden stack member
- **AND** existing modal, drawer and pointer-capture routing retains priority

### Requirement: Paired stack geometry uses one layout walk

Rendering and mouse dispatch SHALL obtain pane frames and stack bars from one
shared layout walk. Callers needing only panes or bars MUST NOT allocate a
discarded vector for the unrequested output.

#### Scenario: A nested stack is rendered or hit-tested

- **WHEN** the caller needs both pane frames and collapsed bars
- **THEN** one layout walk supplies both and agrees with the pane-only and
  bar-only geometry for the same tree and rect

### Requirement: Mouse input targets the pane geometry as drawn

thegn SHALL resolve mouse hits against the pane tree as displayed, which is
the stack while zoomed, and never against hidden split geometry. Selection
anchors and drags, wheel scrolling, pane-app mouse forwarding, and border
gestures MUST use the same rects the renderer drew.

#### Scenario: Selecting text in a zoomed pane

- **WHEN** the user drag-selects inside a zoomed pane
- **THEN** the selection starts at the cell under the pointer in that pane
