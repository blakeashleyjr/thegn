# Agent

## ADDED Requirements

### Requirement: A team verb fans one task across isolated worktrees

thegn SHALL provide a `thegn team` verb that launches a task across multiple
agents, where each teammate MUST run in its own git worktree on its own branch so
that teammates' edits never collide, and MAY run in its own sandbox. The caller's
pane MUST be kept as the orchestrator, and each teammate MUST be launched as a
visible pane rather than a hidden background process.

#### Scenario: Heterogeneous team

- **WHEN** `thegn team "fix the flaky test" --agents claude,codex` runs
- **THEN** thegn creates two worktrees on distinct branches, launches Claude in
  one and Codex in the other as visible panes, and keeps the caller's pane as the
  orchestrator

#### Scenario: Teammates are isolated

- **WHEN** two teammates edit the same file concurrently
- **THEN** each edit lands only in that teammate's own worktree/branch and neither
  overwrites the other

### Requirement: Best-of-N runs the same task in isolated attempts

The team verb SHALL support a best-of-N mode that runs the same task and agent in
N separate worktrees, and the resulting attempts MUST be presentable side by side
in the existing diff/review surface so a human can pick one to merge and discard
the rest.

#### Scenario: N attempts surfaced for comparison

- **WHEN** `thegn team "implement X" --best-of-N 3 --agent claude` runs
- **THEN** three worktrees each attempt the task and their diffs are available to
  compare in the review pane

#### Scenario: Pick one, discard the rest

- **WHEN** the user approves one attempt
- **THEN** that branch merges via the normal merge flow and the other attempt
  worktrees are cleaned up under the dirty guard

### Requirement: A team is observable as a fleet

A launched team SHALL be observable as a fleet roster listing each teammate with
its branch and activity/attention state, and a teammate that raises an attention
signal MUST surface in the roster and the needs-attention queue.

#### Scenario: Teammate finishing raises its hand

- **WHEN** a teammate agent finishes or blocks and raises an attention signal
- **THEN** its roster row reflects the needs-attention state and it is enqueued
  for the one-key jump
