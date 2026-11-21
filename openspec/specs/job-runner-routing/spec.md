# Job runner routing

## Purpose

Define the first typed submission route for owner-configured build and test jobs.

## Requirements

### Requirement: Typed recipes require explicit runner and source identity

The Job contract SHALL accept only allowlisted typed build/test recipes for
selected-runner submissions. A selected recipe MUST name its runner explicitly,
pin the captured source digest, and omit arbitrary command steps. The route MUST
resolve only that configured runner and MUST refuse when it is unavailable or
when the captured source identity differs. It MUST NOT choose a runner from
interactive worktree placement or fall back to local execution.

#### Scenario: Selected recipe reaches configured runner

- **WHEN** a valid typed recipe names an available runner and its pinned source
  matches the captured source
- **THEN** the route submits the recipe and source identity to that runner

#### Scenario: Missing runner or mismatched source

- **WHEN** the named runner is unavailable or the captured digest differs
- **THEN** the route returns a typed refusal before submission

### Requirement: Results identify the submitted operation

The route MUST validate returned runner, recipe and source identity, retain no
more than the bounded log tail, and refuse a result without a terminal exit
code. Provider qualification and stock interactive command interception are
outside this contract.

#### Scenario: Runner cannot prove completion

- **WHEN** a runner returns no terminal exit code
- **THEN** the result is reported as indeterminate rather than success
