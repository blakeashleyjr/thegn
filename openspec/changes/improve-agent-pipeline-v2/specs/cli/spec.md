# CLI

## ADDED Requirements

### Requirement: Session open dispatches a pipeline stage

`thegn session open` SHALL accept `--stage <name> --issue <id>`, which selects
a configured `[[pipeline.stages]]` entry and performs one atomic stage dispatch
(insert the row, render the prompt from the stage's template, refuse an empty
render, open the session — the daemon launch layers the stage's overrides —
stamp the row, mark it running) as specified in the agent spec. `--stage`
WITHOUT `--issue` stays THE-83's overlay open: a plain launch with the stage's
`model` / `env` / `permissions` layered over the agent entry. An explicit
`--prompt` SHALL be refused on the dispatch form (the template owns the task)
and SHALL be honoured on the overlay form. The verb SHALL print the row id,
session id and artifact path it produced, and on failure SHALL name the roster
row it left `failed`.

#### Scenario: A dispatched stage prints its handles

- **WHEN** `session open --stage <name>` succeeds
- **THEN** the output names the roster row id, the daemon session id and the
  sanitized artifact path, and the row is readable back with those values

#### Scenario: A stage whose prompt renders empty is refused

- **WHEN** the stage's rendered prompt is empty
- **THEN** the verb exits non-zero naming the stage, opens no session, and
  leaves no running row

### Requirement: The roster's done outcome is gated at the CLI

`thegn dispatch set-status <id> done` SHALL verify the row's artifact through
the run-completion contract (exists under the worktree, tracked by git) before
writing `done`, SHALL print the refusal reasons verbatim on failure, SHALL
report an uncommitted-worktree state without blocking on it, and SHALL leave
`failed`/`abandoned`/`merged` and artifact-less rows ungated. Every other
status SHALL behave exactly as before.

#### Scenario: An uncommitted artifact refuses done

- **WHEN** a roster row's artifact is written but not committed
- **THEN** `set-status done` exits non-zero and names the artifact with the
  reason to commit it

#### Scenario: A no-artifact row is set done as before

- **WHEN** a row carries no artifact
- **THEN** `set-status done` is not gated and behaves exactly as it did before
  this change

### Requirement: Dispatch verification is a read-only CLI verb

`thegn dispatch verify <id>` SHALL report the run-completion verdict for one
roster row — ok, the artifact path, exists / tracked / dirty flags and the
refusal reasons — without mutating the row, and SHALL be drivable in JSON for a
supervisor's consumption.

#### Scenario: A supervisor checks a claim before recording it

- **WHEN** `dispatch verify` runs against a row whose artifact is missing or
  untracked
- **THEN** the verdict is not-ok with the same reasons the gated
  `set-status done` would print, and the row is unchanged

### Requirement: Dispatch wait blocks on live workers

`thegn dispatch wait` SHALL block until the exit of the session behind one
explicit roster row, or — with `--any` — of every current spawning/running row
that carries a session, composing the routed `sessions.wait` primitive with its
timeout semantics. Selection and refusals SHALL follow the wake primitive's
named errors (no such row / not spawning-running / no session / nothing
active), printed verbatim.

#### Scenario: A wait on a live worker returns at its exit

- **WHEN** `dispatch wait <id>` targets a spawning/running row with a session
- **THEN** the verb blocks in its own process until that session exits and
  reports the row it woke for (id, stage, issue)

#### Scenario: A wait with nothing live refuses honestly

- **WHEN** `dispatch wait --any` finds no spawning/running row with a session
- **THEN** the verb exits non-zero with the nothing-active message rather than
  waiting forever or returning instantly against a parked row

### Requirement: Session liveness is visible and closable by name

`thegn session list` SHALL surface each session's liveness — including exited
sessions with their exit time, exit code and final state from the daemon's
tombstones — and `thegn session list --live` SHALL restrict the listing to
live sessions. `thegn session close <id>` SHALL close a session by name over
the existing `sessions.kill` control capability (no new capability row), so a
supervisor never has to hand-assemble a JSON body against the control plane.

#### Scenario: An exited session is reported, not vanished

- **WHEN** a worker's session exits and the supervisor lists sessions
- **THEN** the exited session still appears with its exit time and exit code
  (tombstone-backed), and `--live` filters it out

#### Scenario: Closing a session is one named verb

- **WHEN** a supervisor wants a worker stopped
- **THEN** `thegn session close <id>` ends that session without the caller
  writing raw control-plane JSON
