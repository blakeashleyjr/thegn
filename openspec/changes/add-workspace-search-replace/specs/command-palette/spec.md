# Command Palette — search & replace handoff

## ADDED Requirements

### Requirement: Content search hands off to the Search & Replace surface

The palette SHALL provide an action (an `ActionSpec` row, per the
every-row-is-an-action contract) that opens the Search & Replace surface, and
from Content mode (`/`) the handoff MUST seed the surface with the current
query so the user escalates from quick search to replace without retyping.
The palette's own Content mode remains search-only and continues to use the
embedded engine.

#### Scenario: Escalating a content query

- **WHEN** the user has typed a Content-mode query in the palette and invokes
  the search-replace action
- **THEN** the palette closes and the Search & Replace surface opens with that
  query pre-filled and searching

#### Scenario: The handoff row is a real action

- **WHEN** the palette lists the search-replace row
- **THEN** it dispatches by an action id in the keymap registry (rebindable,
  shown with its chord), not a string key
