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

### Requirement: Explicit CLI submission from a configured runner

`thegn job run` SHALL submit one allowlisted recipe (`cargo-build` or
`cargo-test`) to exactly one runner, named by `--runner` or else by
`[jobs] runner`, which MUST be an `[env.<name>]` whose provider is `sprites` or
`vercel`. The command MUST pin the digest of the current worktree's snapshot
(the digest the executor verifies), MUST NOT accept a free-form command, and
MUST NOT choose another runner when the named one cannot be built. The runner
MUST be built from the env's own provider configuration; the provider token
MUST stay on the submitting host and MUST NOT enter the recipe's environment.
A missing token, project binding or supported provider MUST fail before any
snapshot is sent and name what is missing. The command MUST refuse to run when
`THEGN_SELECTED_JOB` is set, which the executors set in the recipe environment.
An interrupt SHALL cancel the job through the route's cancel handle. The
process SHALL exit with the recipe's exit code, `130` for a cancel, `124` for a
deadline, `128 + n` for signal `n`, and `1` when no outcome was proven.

#### Scenario: Runner cannot be built

- **WHEN** the selected env has no resolvable token, no bound Sprite or Vercel
  team and project, or an unsupported provider
- **THEN** the command fails with an error naming the env and the missing
  setting, and nothing is submitted

#### Scenario: Started inside a selected job

- **WHEN** `thegn job run` runs with `THEGN_SELECTED_JOB` set
- **THEN** it refuses before any runner is built, any token is resolved or any snapshot is taken

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

### Requirement: Per-project runner binding and route inspector

The selected runner SHALL be resolvable per project. A trusted
`[project.<slug>.jobs] runner` (legacy `[workspace.<slug>.jobs]`) SHALL win over
the global `[jobs] runner` for that project only, and the global key SHALL
remain the fallback. A repo-root `.thegn.*` overlay MUST NOT set it, and an
ambiguous or refused project block MUST fail closed (no runner, not the global
one). An explicit `--runner` SHALL win over both. `thegn job route [--json]`
SHALL print the effective destination and the reason (project binding, global
default, or local) without resolving a token or dispatching anything.

#### Scenario: Two projects bind different runners

- **WHEN** project A binds `[project.a.jobs] runner = "sp"` and project B has
  no binding while `[jobs] runner = "vx"`
- **THEN** `job route` reports `sp` with source `project` for A and `vx` with
  source `global` for B

#### Scenario: Nothing selects a runner

- **WHEN** neither a project binding nor `[jobs] runner` is set
- **THEN** `job route` reports destination `local` and `job run` still
  requires `--runner`
