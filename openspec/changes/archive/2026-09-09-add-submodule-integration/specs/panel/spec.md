# Panel

## ADDED Requirements

### Requirement: Submodule changes render as pointer moves

Changes and drilled diffs SHALL identify submodules separately from files and
show old/new gitlink pointers plus forward, rewind, diverged, or unknown
direction. A bounded local commit summary MAY enrich the move when the objects
exist; absence SHALL degrade to pointers without fetching. Git numstat `-/-`
MUST NOT render as a zero-line edit.

#### Scenario: Offline pointer move stays useful

- **WHEN** the new gitlink object is unavailable locally
- **THEN** the panel shows old and new pointers and no network operation occurs

### Requirement: Panel staging treats gitlinks atomically

The staging UI SHALL offer whole-entry stage/unstage/restore for a submodule
and MUST NOT offer or submit a partial line patch.

#### Scenario: Whole-entry stage

- **WHEN** the user stages a submodule row
- **THEN** the recorded gitlink is staged atomically
