# Panel

## ADDED Requirements

### Requirement: The panel separator drag grabs a two-column band

At the resting width the panel's width SHALL be resizable by dragging its
left separator with the mouse, persisting the settled width as the section's
memory and reporting it like the keyboard routes. The grab target SHALL be a
two-column band — the separator column plus the adjacent pane frame cell
beside it (the second vertical rule at that boundary) — with the extra cell
skipped whenever it is pane or drawer content (`hit_pane`); the separator
column itself always grabs. The divider SHALL hold its grab offset for the
whole drag, so it stays under the cursor instead of jumping to it on the
first sample. A press that never moves MUST change nothing: no width change,
no persist, and no width report — a bare click on the divider is a no-op.
`Esc` while the drag is in flight MUST cancel it, restoring the pre-drag
width and persisting nothing; Esc never half-applies.

#### Scenario: The band grabs the divider or the frame cell beside it

- **WHEN** the user presses on the pane frame cell adjacent to the panel
  separator while that cell is not pane or drawer content
- **THEN** the width drag grabs as if the divider itself had been pressed

#### Scenario: The extra cell yields to pane and drawer content

- **WHEN** the user presses on the band's extra cell in a row where a pane's
  or the drawer's content occupies it
- **THEN** the press reaches that content, not the width drag; only the
  separator column itself grabs there

#### Scenario: A click on the divider changes nothing

- **WHEN** the user presses the separator and releases without moving the
  pointer
- **THEN** no width is stored and the status line does not report a width

#### Scenario: Esc cancels a width drag

- **WHEN** the user presses `Esc` after dragging the separator to a new width
- **THEN** the panel returns to the width it had at the press and nothing is
  persisted
