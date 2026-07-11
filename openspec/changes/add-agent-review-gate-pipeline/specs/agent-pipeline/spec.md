# Agent Pipeline

## ADDED Requirements

### Requirement: Pipeline runs in an ephemeral non-tab worktree

The pipeline SHALL execute its graph in a dedicated ephemeral worktree created via the `GitBackend`, MUST NOT register that worktree as a sidebar tab, and MUST garbage-collect it when the run terminates so the user's open worktrees and working directory are never disturbed.

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

### Requirement: Pipeline is a TOML-authored workflow graph

The pipeline SHALL be modeled as a workflow graph of typed nodes (`agent-exec`, `check`, `approval-gate`, `pr`) wired by typed edges (`sequence`, `conditional-on-severity`, `parallel`, `loop`), authored in layered TOML (`[[pipeline.node]]` / `[[pipeline.edge]]`) using the `config_enum!` idiom; when the `[pipeline]` table is absent the engine MUST instantiate a built-in default graph equal to the linear pipeline intent → review → test → lint → document → approval → PR so behavior is unchanged.

#### Scenario: No pipeline configured

- **WHEN** a pipeline run starts and no `[pipeline]` table is configured
- **THEN** the engine executes the built-in default graph (intent → review → test → lint → document → approval → PR)
- **AND** the node-visit order is exactly that linear chain

#### Scenario: Custom graph configured

- **WHEN** a `[pipeline]` table defines nodes and edges in TOML
- **THEN** the engine executes that graph, traversing each edge per its kind (sequence, conditional-on-severity, parallel, loop)

### Requirement: The graph engine is a pure state machine

The pipeline engine SHALL be a pure, deterministic state machine over the node graph with all I/O injected at its edges, MUST live in `thegn-core` with exhaustive unit tests, and MUST be coverage-gated (95% lines) in the same shape as `render_plan::plan`.

#### Scenario: Deterministic node ordering

- **WHEN** the engine is given a graph, a run state, and an event stream
- **THEN** it returns the next nodes to run (or park/done/failed) with no side effects
- **AND** the same inputs always yield the same node-visit order

#### Scenario: Conditional edge branches on severity

- **WHEN** a node with a `conditional-on-severity` edge produces a finding meeting the edge's severity predicate
- **THEN** the engine traverses that edge to its target (e.g. an approval-gate); otherwise the edge is not taken

### Requirement: Pipeline graph runs off the event loop

The pipeline SHALL execute its graph off the event loop, MUST report node transitions and findings over a channel that pulses the `TerminalWaker`, and MUST NOT introduce a polling timeout or blocking I/O on the loop.

#### Scenario: Node produces output

- **WHEN** a node emits a finding or changes state
- **THEN** it sends on the mpsc channel and pulses the waker
- **AND** the loop re-renders only when dirty, with no added tick

### Requirement: Pipeline degrades without AI

The pipeline SHALL run with AI strictly additive, MUST execute the deterministic `check` nodes (the configured test/lint/fmt commands plus pre-commit hooks) when no agent or proxy is configured, and MUST skip every `agent-exec` node rather than fail.

#### Scenario: No agent or proxy configured

- **WHEN** a pipeline run starts with no AI layer available
- **THEN** only the deterministic `check` nodes and pre-commit hooks run
- **AND** the `agent-exec` nodes (intent, AI review, change explanation) are skipped and the run can still reach the `pr` node
