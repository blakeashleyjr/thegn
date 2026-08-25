# Capability Catalog — deltas

## ADDED Requirements

### Requirement: CI runs and job logs are readable over the control surfaces

The catalog SHALL carry two read-scoped CI rows — `ci.runs` (a worktree's
cached normalized run history with its `fetched_at` staleness) and `ci.log`
(one cached, redacted job log with its first-failure line and
truncated/redacted indicators) — projected per the catalog's normal
surface-coverage rules: HTTP and CLI implemented (`thegn ci runs --json`,
`thegn ci log` claim the CLI cells), MCP implemented as parameterised state
tools (`ci_runs`, `ci_log`), and gRPC/plugin excused in `SURFACE_GAPS` until
mirrored. Both rows answer from the state-DB caches via the daemon —
unparseable cache rows are skipped, and a `ci.log` cache miss MUST return a
distinct not-cached error rather than invoking a provider CLI from the
daemon. Their scope is `required_scope(verb)` like every row; no second
policy table. `ci.log` arguments SHALL default so that a call naming only a
worktree resolves to the latest failed run's first failing job.

#### Scenario: An MCP agent reads the latest failure with one call

- **WHEN** an MCP client with the read scope calls `ci_log` with only a
  worktree argument
- **THEN** it receives the latest failed run's first failing job's redacted
  cached log with its first-failure line, or a not-cached error naming how
  the cache gets populated

#### Scenario: Scope gating matches the catalog

- **WHEN** a client whose token or `--scopes` grant lacks the read scope
  calls `ci_runs` or `ci_log` on any surface
- **THEN** the call is refused naming the missing scope, per the same
  `required_scope` table every door checks

#### Scenario: The daemon never shells out for a log

- **WHEN** `ci.log` is requested for a job that is not in the cache
- **THEN** the daemon returns the not-cached error and spawns no vendor CLI
