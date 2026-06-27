# Agent Pipeline

## ADDED Requirements

### Requirement: Pipeline runs in an ephemeral non-tab worktree

The pipeline SHALL execute its stages in a dedicated ephemeral worktree created via the `GitBackend`, MUST NOT register that worktree as a sidebar tab, and MUST garbage-collect it when the run terminates so the user's open worktrees and working directory are never disturbed.

#### Scenario: Pipeline starts

- **WHEN** a pipeline run begins for a branch
- **THEN** a worktree under the reserved ephemeral naming scheme is created off the user's tabs
- **AND** it does not appear in the workspace tree

#### Scenario: Pipeline finishes or is cancelled

- **WHEN** a run reaches a terminal state (done, failed, or cancelled)
- **THEN** its ephemeral worktree is removed via the stale-worktree cleanup seam

#### Scenario: Orphaned worktree after a crash

- **WHEN** a run's process dies without cleanup
- **THEN** the reserved-prefix worktree is reclaimed by the same garbage collector on a later sweep

### Requirement: Pipeline stages run in order off the event loop

The pipeline SHALL run an ordered sequence of stages (review, test, lint, document, PR) off the event loop, MUST report stage transitions and findings over a channel that pulses the `TerminalWaker`, and MUST NOT introduce a polling timeout or blocking I/O on the loop.

#### Scenario: Stage produces output

- **WHEN** a stage emits a finding or changes state
- **THEN** it sends on the mpsc channel and pulses the waker
- **AND** the loop re-renders only when dirty, with no added tick

### Requirement: Pipeline degrades without AI

The pipeline SHALL run with AI strictly additive, MUST execute the deterministic non-AI checks (the configured test/lint/format commands plus pre-commit hooks) when no agent or proxy is configured, and MUST skip the AI stages rather than fail.

#### Scenario: No agent or proxy configured

- **WHEN** a pipeline run starts with no AI layer available
- **THEN** only the deterministic test/lint/format checks and pre-commit hooks run
- **AND** the AI stages (intent, AI review, change explanation) are skipped and the run can still open a PR
