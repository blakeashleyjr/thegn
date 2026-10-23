# CI Inspection

## Purpose

thegn surfaces CI/CD pipeline state — run history, job/step drilldown, and logs
with jump-to-failure — across providers through a provider-agnostic model. CI is a
separate axis from the forge: the `CiProvider` trait is a sibling of the forge
trait, providers degrade native-API → CLI → unavailable, and the whole layer is
AI-free.

## Requirements

### Requirement: Provider-agnostic run/job/step/log model

CI SHALL be modeled as a normalized `run → job → step → log` shape behind a `CiProvider` trait, and each provider MUST degrade native-API → CLI → unavailable, with mutating operations capability-gated.

#### Scenario: Provider lacks native API

- **WHEN** a provider has no native API available but a CLI is present
- **THEN** the provider serves runs/jobs/logs via the CLI

#### Scenario: Mutation not supported

- **WHEN** a trigger/rerun/cancel is requested on a provider that does not declare
  the capability
- **THEN** the mutation is refused rather than attempted

### Requirement: CI rollup in the panel and statusbar

CI state SHALL appear as a panel `Ci` section (recent runs + per-run state + summary chip) and a statusbar badge that is red on failure, amber while running, and silent when green.

#### Scenario: A run is failing

- **WHEN** the latest CI run has failures
- **THEN** the statusbar shows a red CI badge and the panel section reflects the
  failure

### Requirement: CI refresh runs off the event loop

CI cache refresh SHALL run off-loop (`spawn_blocking` + channel + `TerminalWaker`) on switch and on a bounded interval, writing the `ci_runs_cache`, preserving the ~0% idle invariant.

#### Scenario: Background CI poll

- **WHEN** CI data is refreshed
- **THEN** it runs off-loop and wakes the loop only with new data, adding no
  polling timeout

### Requirement: "Why did it fail" is AI-free

Failure explanation SHALL be a log scan that marks the first failure line
(error markers / exit codes / panics), with no LLM involvement. The same
deterministic marker SHALL anchor the redacted log excerpt handed to the
agent-task engine and reported in the `ci.logs` capability payload — the
optional agent handoff consumes the scan's output and never replaces it.

#### Scenario: First-failure marker

- **WHEN** the user views a failed run's log
- **THEN** a "first failure at line N" marker is shown from a deterministic
  scan

#### Scenario: The excerpt is anchored on the marker

- **WHEN** a `pr_ci_failure` task prompt or a `ci.logs` payload is built for a
  failed job
- **THEN** its excerpt window and reported failure line come from the same
  deterministic scan

### Requirement: The CI provider is a sync, object-safe seam

`CiProvider` SHALL use plain `&self` methods (every implementation is process-bound), SHALL implement `Probe` and report its `CiSystem`, and provider selection SHALL return `Box<dyn CiProvider>` — no delegation enum and no runtime handle at call sites.

#### Scenario: A CI call from a blocking thread

- **WHEN** the host fetches runs from `spawn_blocking`
- **THEN** it calls the provider directly with no `block_on`

### Requirement: CI run JSON reports degraded provider results

`thegn ci runs --json` SHALL emit a JSON array when the provider returns a
complete run list. On a provider error it SHALL emit a JSON error object; when
the provider returns malformed rows it SHALL emit an error object containing
`discarded_rows`. Both degraded outcomes SHALL retain exit code 0.

#### Scenario: Provider returns malformed rows

- **WHEN** `thegn ci runs --json` receives a partial run list
- **THEN** stdout contains a JSON error object with `discarded_rows` and the
  command exits 0

### Requirement: Failing job logs are a cached first-class resource

When a CI run reaches a terminal failed state, thegn SHALL fetch and cache the
failing jobs' log tails off the event loop (bounded by `[ci] log_tail_lines`
and a `[ci] log_cache_runs` retention count, `0` disabling the cache), and
every log consumer — the drill overlay, `thegn ci logs` (with `ci log` as a
compatibility alias), and the control/MCP surfaces — SHALL serve a terminal
run's log from the cache without
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
(`thegn_core::ci_log::redact`) **before being written to the log cache**, masking
secret-shaped content (provider token prefixes, AWS key ids, JWTs, PEM blocks,
`Authorization:` header values, URL userinfo credentials, and
credential-named `key = value` assignments), so that no surface — TUI, CLI,
control API, MCP, or an agent prompt — ever receives unscrubbed cached log
text and secrets never rest in the state DB. The response MUST carry an
indication that lines were redacted and/or the tail truncated.

#### Scenario: A token in a log is masked everywhere

- **WHEN** a fetched job log contains a `ghp_…` token on a line
- **THEN** the cached text carries `***redacted***` in its place, and the
  drill view, `thegn ci logs` (with `ci log` as a compatibility alias), the
  `ci.logs` capability payload, and any agent
  prompt excerpt all show the masked form

#### Scenario: Only the chokepoint writes the cache

- **WHEN** any code path stores CI log text in the log cache
- **THEN** it does so via the redaction chokepoint (pinned by a test that no
  other writer exists)

### Requirement: A failed run can be handed to the headless agent engine under an explicit policy

thegn SHALL support handing a failed CI run to the shared agent-task engine as
a `pr_ci_failure` task (prompt variables: branch, worktree, workflow, run id and
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
