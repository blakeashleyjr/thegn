# Selected runner routing

## ADDED Requirements

### Requirement: Typed recipes require explicit runner and source identity

The Job contract SHALL accept only allowlisted typed build/test recipes for
selected-runner submissions. A selected recipe MUST name its runner explicitly,
pin the captured source digest, and omit arbitrary command steps. The route MUST
forward the spec deadline to the runner. A recipe spec MUST leave every field
the route cannot forward at its default; submission MUST fail otherwise instead
of silently ignoring a value. The route MUST resolve only that configured runner
and MUST refuse when it is unavailable or when the captured source identity
differs. It MUST NOT choose a runner from interactive worktree placement or fall
back to local execution.

#### Scenario: Selected recipe reaches configured runner

- **WHEN** a valid typed recipe names an available runner, its pinned source
  matches the captured source, and its timeout is set
- **THEN** the route submits the recipe, source identity and deadline to that
  runner

#### Scenario: A field the route cannot forward is set

- **WHEN** a recipe spec sets env, secrets, services, caches, shards, resources,
  isolation, placement, artifacts, results, retry, or source.exclude
- **THEN** validation fails before any runner call and names the field

#### Scenario: Missing runner or mismatched source

- **WHEN** the named runner is unavailable or the captured digest differs
- **THEN** the route returns a typed refusal before submission

### Requirement: Results identify the submitted operation

The route MUST validate returned runner, recipe and source identity. A successful
exit, nonzero exit, cancellation, deadline timeout and signal termination SHALL
read back through the existing `JobResult` shape as succeeded, failed,
cancelled, timed out and failed respectively. A refusal raised before the runner
call SHALL read back as rejected. A runner call that yields no trustworthy
terminal receipt — a transport failure, an unproven completion, or a mismatched
receipt — SHALL read back as indeterminate, never as rejected, because the
recipe may already have run. The retained log tail SHALL be bounded and SHALL
report truncation when earlier output is discarded. Provider qualification and
stock interactive command interception are outside this contract.

#### Scenario: Runner cannot prove completion

- **WHEN** the runner call fails after submission or a runner returns no
  terminal exit code
- **THEN** the result is reported as indeterminate rather than rejected or
  succeeded

#### Scenario: Selected ordinary build is read back through JobResult

- **WHEN** a project-selected `cargo-build` recipe returns exit code zero, a
  nonzero code, cancellation, a deadline timeout or a signal
- **THEN** the user-facing result is respectively `Succeeded`, `Failed`,
  `Cancelled`, `TimedOut` or `Failed`, preserving the bounded log tail

#### Scenario: Selected route is unavailable

- **WHEN** a project-selected recipe names a runner that is not available
- **THEN** the user-facing result is `Rejected`, with no placement-derived
  fallback or fabricated provider/run metadata

#### Scenario: Output exceeds the retained tail

- **WHEN** the runner returns more than 16 KiB of output
- **THEN** only the tail is retained and the result reports
  `truncated.logTail`
