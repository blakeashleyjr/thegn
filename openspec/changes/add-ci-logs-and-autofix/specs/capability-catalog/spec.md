# Capability Catalog — deltas

## ADDED Requirements

### Requirement: CI runs and job logs are readable over the control surfaces

The catalog SHALL carry two read-scoped CI rows — `ci.runs` (a worktree's
cached normalized run history with its `fetched_at` staleness) and `ci.logs`
(one cached, redacted job log with its first-failure line and
truncated/redacted indicators) — projected per the catalog's normal
surface-coverage rules: HTTP and CLI implemented (`thegn ci runs --json`,
`thegn ci logs` (with `ci log` as a compatibility alias) claim the CLI cells), MCP implemented as
parameterised state
tools (`ci_runs`, `ci_logs`), and gRPC/plugin excused in `SURFACE_GAPS` until
mirrored. Both rows answer from the state-DB caches via the daemon —
unparseable cache rows are skipped, and a `ci.logs` cache miss falls back
to a provider fetch on the daemon's blocking provider lane. Their scope is
`required_scope(verb)` like every row; no second
policy table. `ci.logs` arguments SHALL default so that a call naming only a
worktree resolves to the latest failed run's first failing job.

#### Scenario: An MCP agent reads the latest failure with one call

- **WHEN** an MCP client with the read scope calls `ci_logs` with only a
  worktree argument
- **THEN** it receives the latest failed run's first failing job's redacted
  cached log with its first-failure line, or a provider fetch result on a
  cache miss

#### Scenario: Scope gating matches the catalog

- **WHEN** a client whose token or `--scopes` grant lacks the read scope
  calls `ci_runs` or `ci_logs` on any surface
- **THEN** the call is refused naming the missing scope, per the same
  `required_scope` table every door checks

#### Scenario: A cache miss fetches the log off the daemon event loop

- **WHEN** `ci.logs` is requested for a job that is not in the cache
- **THEN** the daemon fetches the log on its blocking provider lane, keeping
  provider I/O off the event loop
