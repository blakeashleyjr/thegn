# Sidebar

## MODIFIED Requirements

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

## ADDED Requirements

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
