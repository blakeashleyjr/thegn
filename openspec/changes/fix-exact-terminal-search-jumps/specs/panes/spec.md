# Terminal history search

## ADDED Requirements

### Requirement: Enter jumps to the exact terminal line

Activating a terminal-history search result SHALL scroll the pane to the grid
line the result was ranked from, even when the same logical text appears more
than once and earlier output wraps. The jumped line MUST be highlighted.

#### Scenario: Duplicate text after wrapped output

- **WHEN** a search result matches text that also appears earlier, and output
  before the older copy wrapped across several physical rows
- **THEN** Enter scrolls to and highlights the occurrence at the result's
  logical distance from the bottom

#### Scenario: Line has left the emulator

- **WHEN** the result's line is no longer held by the emulator, or came from a
  full-screen app
- **THEN** the pane is not scrolled to an unrelated line and the status names
  the unavailable line and points at replay

### Requirement: Stepping through hits after a jump

After Enter activates a result, the ranked hits SHALL remain available until
the next key or scroll. `n` and `N` MUST move to the next and previous hit,
wrapping at either end, and update the highlight and the 1-based position.

#### Scenario: Step forward and wrap

- **WHEN** the user presses `n` after a jump
- **THEN** the next ranked hit is highlighted, and `n` on the last hit returns
  to the first

#### Scenario: Step backward

- **WHEN** the user presses `N` after a jump
- **THEN** the previous ranked hit is highlighted, wrapping from the first hit
  to the last

#### Scenario: End the walk

- **WHEN** the user presses a key other than `n`/`N`, scrolls the wheel over a
  pane, or a press invalidates the selection
- **THEN** the mark is cleared and `n`/`N` no longer step the old results
