# Panes

## ADDED Requirements

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
