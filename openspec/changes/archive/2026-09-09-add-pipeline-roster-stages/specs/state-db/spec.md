# State DB

## ADDED Requirements

### Requirement: The dispatch roster records pipeline structure

The `agent_dispatches` roster SHALL carry, per row, the pipeline stage it
belongs to, the row it was chunked out of, the session running it, and the path
of its handoff artifact. All four are optional: a dispatch made outside a
pipeline, and every row written before the columns existed, MUST read back as
absent with no change in behaviour. The artifact field SHALL hold a **pointer**
to a file committed in the worktree, never the artifact's content — git remains
the source of truth for what a stage produced.

These columns are **structure, not judgment**: the system SHALL store, group and
render them, and MUST NOT advance a stage, enforce a stage's concurrency, or
expire a stage on a timeout. Stage transitions belong to the supervising agent.

#### Scenario: A pre-existing database gains the columns without losing data

- **WHEN** a state database written before the pipeline columns existed is
  opened
- **THEN** every existing dispatch row survives with its issue, worktree, agent
  and status intact, its pipeline fields read as absent, and the schema version
  stamp advances

#### Scenario: A chunk row records its parent and stage

- **WHEN** a supervisor records a dispatch with a stage, a parent row, a session
  and an artifact path
- **THEN** reading the roster back returns all four alongside the existing
  fields, from both the whole-roster read and the by-id read

#### Scenario: The roster is not a scheduler

- **WHEN** a stage's dispatch reaches a terminal status
- **THEN** no row's stage advances on its own — the next stage exists only when
  a supervisor records it

### Requirement: A finished worker's outcome is attributed to its own row

When a worker finishes, the system SHALL resolve which roster row it was by its
recorded session first, and only otherwise by the most recent **active** row for
its worktree. Rows in a terminal status MUST NOT be selected, and when no row
matches the outcome MUST be treated as "not an agent worker" rather than as an
error or a guess.

#### Scenario: Two stages share one worktree

- **WHEN** two active dispatches exist for the same worktree and one of their
  workers finishes
- **THEN** the outcome is stamped on the row whose session ran that worker, not
  on whichever row is newest

#### Scenario: A finished row is not re-stamped

- **WHEN** an ordinary program exits in a worktree whose dispatches have all
  reached a terminal status
- **THEN** no roster row is modified and no agent-finished notification is
  raised; the exit is handled as an ordinary process exit

#### Scenario: A worker with no recorded session

- **WHEN** a worker launched without a recorded session finishes in a worktree
  with one active dispatch
- **THEN** that active row receives the outcome
