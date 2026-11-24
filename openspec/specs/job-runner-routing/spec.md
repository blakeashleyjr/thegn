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

The route MUST validate returned runner, recipe and source identity and retain
no more than the bounded log tail. A selected runner's successful exit,
nonzero exit, and cancellation SHALL read back through the existing
`JobResult` shape as succeeded, failed, and cancelled respectively. A
pre-submission refusal SHALL read back as rejected. An unproven terminal result
or mismatched runner receipt SHALL read back as indeterminate. Provider
qualification and stock interactive command interception are outside this
contract.

#### Scenario: Runner cannot prove completion

- **WHEN** a runner returns no terminal exit code
- **THEN** the result is reported as indeterminate rather than success

#### Scenario: Selected ordinary build is read back through JobResult

- **WHEN** a project-selected `cargo-build` recipe returns exit code zero,
  a nonzero code, or cancellation
- **THEN** the user-facing result is respectively `Succeeded`, `Failed`, or
  `Cancelled`, preserving the bounded log tail

#### Scenario: Selected route is unavailable

- **WHEN** a project-selected recipe names a runner that is not available
- **THEN** the user-facing result is `Rejected`, with no placement-derived
  fallback or fabricated provider/run metadata
