# Agent Orchestration

## ADDED Requirements

### Requirement: Race one prompt across N worktrees

The racer SHALL fan a single prompt out to N agents, each in its own worktree created via the `GitBackend` worktree path, MUST reuse the proxy best-of-N fan-out for the racing model traffic rather than introducing a parallel fan-out, and MUST key race membership on the existing `AgentDispatch` / `agent_dispatches` seed records.

#### Scenario: Start a race

- **WHEN** a user starts a race with prompt P and degree N
- **THEN** N worktrees are created and N agents are dispatched against P
- **AND** each worker is tracked by its seed `AgentDispatch` record

#### Scenario: Racing model traffic uses the proxy fan-out

- **WHEN** the racing agents issue model requests
- **THEN** those requests go through the proxy best-of-N fan-out (AR 563), not a separate mechanism

### Requirement: Side-by-side compare and cherry-pick into a merge worktree

When the racers finish, the user SHALL be able to compare the N worktrees side by side in the existing diff/review pane and cherry-pick selected hunks into a dedicated merge worktree; the cherry-pick MUST use the existing git mutations and MUST NOT modify local main.

#### Scenario: Compare results

- **WHEN** two or more racing worktrees have produced changes
- **THEN** their diffs are shown side by side in the diff/review pane (T 267)

#### Scenario: Cherry-pick the best into a merge worktree

- **WHEN** the user selects hunks from one or more racing worktrees
- **THEN** those hunks are cherry-picked into a dedicated merge worktree via the existing `gitmut.rs` mutations
- **AND** local main is left unchanged

### Requirement: Typed orchestration message protocol

The orchestration layer SHALL emit typed messages in the set {`status`, `dispatch`, `worker_done`, `decision_gate`}, support `@group` addressing to a fleet, render clickable task links through the existing panel link-activation path, and seed message state from the existing `AgentDispatch` record.

#### Scenario: Worker reports completion

- **WHEN** a racing or scheduled worker finishes
- **THEN** it emits a `worker_done` message addressed to its `@group`
- **AND** the fleet status reflects the change

#### Scenario: Clickable task link

- **WHEN** a message carries a task reference
- **THEN** the rendered link is clickable and activates through the existing panel link-activation path

#### Scenario: Decision gate blocks fan-in

- **WHEN** a `decision_gate` message is open for a fleet
- **THEN** fan-in waits until the gate is resolved
- **AND** resolving the gate advances the run

### Requirement: Orchestration runs off the event loop

All orchestration producers (racer workers, the scheduler, dispatch transitions) SHALL send on an mpsc channel and pulse the `TerminalWaker`, and MUST NOT add a polling timeout, tick, or blocking I/O to the main loop.

#### Scenario: Idle wake stays Skip

- **WHEN** the loop wakes with no orchestration message and no due task
- **THEN** `render_plan::plan` returns `Skip` and the 0%-idle contract holds

#### Scenario: Streaming racer output

- **WHEN** a racing agent streams output into its worktree's pane and nothing else is dirty
- **THEN** the frame is `Panes` (a bounded `diff_region`), not a chrome recompose

#### Scenario: Fleet status change

- **WHEN** a `status` / `worker_done` / `decision_gate` message changes the fleet badge, dispatch chip, or sidebar dot
- **THEN** the master chrome `dirty` is set and the frame is `Full`

### Requirement: Scheduled prompt runs

A scheduled task SHALL be definable with a schedule (a preset, a raw cron expression, or an RRULE) and an IANA timezone, MUST target either a repo (creating a fresh worktree) or an existing worktree, MAY set `--reuse-session` to continue in the same live terminal, and MUST be persisted in the `scheduled_tasks` table.

#### Scenario: Schedule against a repo

- **WHEN** a task is scheduled with a repo target and no worktree
- **THEN** each fire creates a fresh worktree and dispatches the prompt there

#### Scenario: Reuse an existing session

- **WHEN** a task targets an existing worktree with `--reuse-session`
- **THEN** the fire continues the prompt in that same live terminal rather than creating a new worktree

#### Scenario: Timezone-aware firing

- **WHEN** a task's schedule and IANA timezone are evaluated
- **THEN** the next fire time is computed in that timezone

### Requirement: Scheduled tasks are created disabled and gated to enable

A scheduled task SHALL be created in the disabled state, MUST support a manual test-trigger (fire-now) while disabled, and MUST NOT fire on its schedule until explicitly enabled.

#### Scenario: Newly created task does not fire

- **WHEN** a scheduled task is first created
- **THEN** it is disabled and its schedule does not fire

#### Scenario: Test-trigger before enabling

- **WHEN** the user manually test-triggers a disabled task
- **THEN** the prompt fires once for verification without enabling the schedule

#### Scenario: Enable after verification

- **WHEN** the user enables a verified task
- **THEN** its `next_fire_at` is computed and it fires on schedule thereafter

### Requirement: Orchestration is AI-additive and never a hard dependency

The orchestration layer SHALL be strictly additive: the AI-free shell MUST build and run with orchestration absent or disabled, and when no agent or proxy is configured the scheduling, racing, and message surfaces MUST be inert (persisting state and rendering as disabled) rather than erroring or blocking the shell.

#### Scenario: No agent or proxy configured

- **WHEN** orchestration surfaces are reached with no agent or proxy configured
- **THEN** they render disabled and persisted rows remain intact
- **AND** the shell does not error or block

#### Scenario: Orchestration layer absent

- **WHEN** the shell runs without the orchestration layer compiled or enabled
- **THEN** all non-AI shell behavior works unchanged
