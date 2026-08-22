# Sidebar

## MODIFIED Requirements

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
