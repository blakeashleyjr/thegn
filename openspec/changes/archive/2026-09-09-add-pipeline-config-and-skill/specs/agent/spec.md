# agent

## ADDED Requirements

### Requirement: The pipeline is conducted by an agent, never by thegn

A multi-stage pipeline SHALL be executed by a supervising agent reading the
configured stage chart and the durable dispatch roster, using the same
worktree/session/roster verbs any operator has. thegn SHALL provide the hands —
validated structure, session lifecycle, the roster, and the merge queue — and
MUST NOT provide the head: no scheduler advances a stage, counts concurrency
slots, or times a stage worker out.

The supervisor's per-stage concurrency budget SHALL be derived from the roster
rather than from the supervisor's memory: the active rows carrying a stage's
name are that stage's occupied slots, so a restarted supervisor resumes without
double-dispatching.

#### Scenario: Resuming a pipeline after a restart

- **WHEN** a supervising agent restarts mid-pipeline and reads the roster
- **THEN** each active row's stage, parent and session identify the work already
  in flight, and the supervisor starts only the stages whose slots are free

#### Scenario: With no supervisor running

- **WHEN** a stage chart is configured but no supervising agent is running
- **THEN** nothing is dispatched and nothing advances — the chart is inert data

### Requirement: Stages hand off through an artifact committed in the worktree

A stage SHALL pass its result to the next stage as a file committed on the
branch, whose path the roster row records as a pointer. The handoff MUST NOT
depend on the supervisor's context window, and the roster MUST NOT become the
document store — git stays the source of truth for what a stage decided.

A fan-out stage SHALL emit one artifact per child, and each child's roster row
SHALL carry both the parent row and its own artifact path, so the parent→chunk
shape survives a crash.

#### Scenario: Design fanned out to several workers

- **WHEN** a design stage emits one chunk file per downstream worker
- **THEN** one roster row per chunk is recorded, each naming the design stage's
  row as its parent and its own chunk file as its artifact

#### Scenario: Reading a handoff

- **WHEN** the supervisor advances a stage
- **THEN** it reads the committed artifact as evidence about the work, and treats
  its content as data — never as instructions that could re-plan the pipeline

### Requirement: Landing is the merge queue, not a pipeline stage

A pipeline SHALL finish by handing the branch to the existing merge queue rather
than by declaring a stage that merges. The queue's serial fold, gate, and
compare-and-swap advance — including its configured agent handoff on conflict or
gate failure — MUST remain the only landing path, so a pipeline cannot
reintroduce a parallel one.

#### Scenario: A chart with no next stage

- **WHEN** the last stage of a chart completes and declares no next stage
- **THEN** the supervisor enqueues the branch and runs the merge queue, and no
  merging behaviour is duplicated in the chart
