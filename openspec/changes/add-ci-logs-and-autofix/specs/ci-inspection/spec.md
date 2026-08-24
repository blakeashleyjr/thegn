# CI Inspection — deltas

## ADDED Requirements

### Requirement: Failing job logs are a cached first-class resource

When a CI run reaches a terminal failed state, thegn SHALL fetch and cache the
failing jobs' log tails off the event loop (bounded by `[ci] log_tail_lines`
and a `[ci] log_cache_runs` retention count, `0` disabling the cache), and
every log consumer — the drill overlay, `thegn ci log`, and the control/MCP
surfaces — SHALL serve a terminal run's log from the cache without
re-fetching. A cache miss for an in-flight or uncached job MAY fall through to
a live provider fetch on the TUI/CLI paths; retention MUST evict oldest-run
logs per worktree so the table stays bounded.

#### Scenario: A run fails and its log is cached once

- **WHEN** the off-loop CI refresh observes a run transition to a terminal
  failed state
- **THEN** the failing jobs' log tails are fetched via the `CiProvider` seam,
  written to the log cache, and subsequent drill/CLI/API reads of that run's
  logs are served from the cache with no provider call

#### Scenario: Retention bounds the cache

- **WHEN** logs for more than `log_cache_runs` failed runs of one worktree
  have been cached
- **THEN** the oldest runs' log rows are evicted on write

### Requirement: Log content is redacted before it is stored or leaves the process

CI log text SHALL pass through a single pure redaction chokepoint
(`thegn_core::ci_redact`) **before being written to the log cache**, masking
secret-shaped content (provider token prefixes, AWS key ids, JWTs, PEM blocks,
`Authorization:` header values, URL userinfo credentials, and
credential-named `key = value` assignments), so that no surface — TUI, CLI,
control API, MCP, or an agent prompt — ever receives unscrubbed cached log
text and secrets never rest in the state DB. The response MUST carry an
indication that lines were redacted and/or the tail truncated.

#### Scenario: A token in a log is masked everywhere

- **WHEN** a fetched job log contains a `ghp_…` token on a line
- **THEN** the cached text carries `***redacted***` in its place, and the
  drill view, `thegn ci log`, the `ci.log` capability payload, and any agent
  prompt excerpt all show the masked form

#### Scenario: Only the chokepoint writes the cache

- **WHEN** any code path stores CI log text in the log cache
- **THEN** it does so via the redaction chokepoint (pinned by a test that no
  other writer exists)

### Requirement: A failed run can be handed to the headless agent engine under an explicit policy

thegn SHALL support handing a failed CI run to the shared agent-task engine as
a `ci_failure` task (prompt variables: branch, worktree, workflow, run id and
URL, job name, and a redacted log excerpt centered on the first failure line),
governed by `[ci.autofix] mode = "off" | "suggest" | "auto"` (default `off`).
In `suggest` mode a failure MUST only raise an actionable notification plus a
fix action requiring a human keypress. In `auto` mode dispatch MUST
additionally require that the run's head SHA equals the worktree's current
HEAD, that a per-head-SHA attempt budget is not exhausted, and that the branch
is owned by neither the PR queue nor the merge queue. The prompt excerpt MUST
come from the redacted cache, never a fresh fetch, and the shell MUST remain
fully functional with no agent configured.

#### Scenario: Suggest mode waits for the human

- **WHEN** `mode = "suggest"` and a run fails for a worktree's current branch
- **THEN** a notification and a fix action appear, and no agent runs until
  the user invokes the action

#### Scenario: Auto mode refuses a stale or owned branch

- **WHEN** `mode = "auto"` and a run fails whose head SHA is not the
  worktree's HEAD, or whose branch is enqueued in the PR queue or merge queue
- **THEN** no agent is dispatched (the PR/merge queue keeps sole ownership of
  its entries)

#### Scenario: The attempt budget bounds retries

- **WHEN** an auto dispatch for a head SHA has already consumed the configured
  attempts
- **THEN** further failures for that SHA only notify; a new head SHA refills
  the budget

### Requirement: Local workflow execution is a configured tool, not an embedded engine

thegn SHALL NOT embed a local workflow executor or expose a local-execution CI
provider; running workflows locally (act, wrkflw) SHALL be supported as
documented `[[tools]]` entries that launch in a pane in the worktree under the
ordinary pane sandboxing, with a recipe in the example config and help.

#### Scenario: Running act locally

- **WHEN** a user configures act or wrkflw as a `[[tools]]` entry and invokes
  it from the picker
- **THEN** it runs as a plain pane command in the active worktree, and no
  thegn code path interprets or executes workflow definitions itself

## MODIFIED Requirements

### Requirement: "Why did it fail" is AI-free

Failure explanation SHALL be a log scan that marks the first failure line
(error markers / exit codes / panics), with no LLM involvement. The same
deterministic marker SHALL anchor the redacted log excerpt handed to the
agent-task engine and reported in the `ci.log` capability payload — the
optional agent handoff consumes the scan's output and never replaces it.

#### Scenario: First-failure marker

- **WHEN** the user views a failed run's log
- **THEN** a "first failure at line N" marker is shown from a deterministic
  scan

#### Scenario: The excerpt is anchored on the marker

- **WHEN** a `ci_failure` task prompt or a `ci.log` payload is built for a
  failed job
- **THEN** its excerpt window and reported failure line come from the same
  deterministic scan
