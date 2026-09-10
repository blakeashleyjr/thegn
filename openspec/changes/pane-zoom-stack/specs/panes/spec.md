# Panes — zoom delta

## ADDED Requirements

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

### Requirement: Mouse input targets the pane geometry as drawn

thegn SHALL resolve mouse hits against the pane tree as displayed, which is
the stack while zoomed, and never against hidden split geometry. Selection
anchors and drags, wheel scrolling, pane-app mouse forwarding, and border
gestures MUST use the same rects the renderer drew.

#### Scenario: Selecting text in a zoomed pane

- **WHEN** the user drag-selects inside a zoomed pane
- **THEN** the selection starts at the cell under the pointer in that pane
