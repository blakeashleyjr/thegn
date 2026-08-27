# Control Plane

## ADDED Requirements

### Requirement: The dispatch put verb carries the pipeline columns

The `dispatches.put` payload SHALL accept the pipeline fields — stage, parent
row, session, artifact path — as optional, default-absent fields, and the created
row returned to the caller SHALL include them. No additional verb, capability row
or scope SHALL be introduced for them: one append-only writer carries the whole
row, so no mutable stage field exists on the wire for the system to advance.

#### Scenario: A client written before the fields exist

- **WHEN** a client posts a payload carrying only issue, worktree and agent
- **THEN** the call succeeds unchanged and the created row's pipeline fields are
  absent

#### Scenario: A pipeline dispatch over the control plane

- **WHEN** a client posts a payload including the pipeline fields
- **THEN** the created row is returned carrying every one of them
