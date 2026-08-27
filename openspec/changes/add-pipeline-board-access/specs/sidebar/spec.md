# Sidebar

## ADDED Requirements

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
