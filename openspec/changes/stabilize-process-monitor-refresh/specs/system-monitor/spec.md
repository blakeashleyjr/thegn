## ADDED Requirements

### Requirement: Process snapshots survive unrelated hydration

The latest process snapshot SHALL remain available across Git/DB model hydration
until the process sampler supplies a replacement. Hydration MUST NOT create an
empty process-table interval or reset its cursor between process samples.

#### Scenario: Hydration lands between process samples

- **WHEN** a populated process snapshot is displayed and unrelated model hydration arrives
- **THEN** its rows, sampled values, selection, and viewport remain available without a sampling placeholder
- **AND** a later process sample replaces the snapshot normally

### Requirement: Process refresh ordering and identity remain coherent

The bounded CPU/RSS retained set and flat/tree row ordering SHALL resolve equal
sort keys deterministically by PID. Passive refresh SHALL retain the selected
sampled process identity when it remains in the retained list, and otherwise
clamp the old cursor position to the remaining rows. Explicit sort and filter
commands SHALL retain their existing selection semantics. Manual wheel scrolling
MUST NOT be pulled back to the selection by a passive sample.

#### Scenario: Many idle processes tie at the retained cutoff

- **WHEN** process enumeration returns more tied CPU/RSS rows than the retained limit in a different order
- **THEN** the retained set and displayed tie order remain identical

#### Scenario: The selected process changes rank

- **WHEN** a passive process sample changes the selected process's sorted position
- **THEN** the selection follows its sampled PID and birth time
- **AND** the visible selected row and signal prompt refer to that same sampled process

#### Scenario: A pending signal target changes sampled identity

- **WHEN** the process identity named by a pending confirmation has vanished or has a different sampled birth time in the latest displayed snapshot
- **THEN** confirming refuses the action and asks for a fresh selection
- **AND** a reused PID does not inherit an earlier process's signal escalation state
