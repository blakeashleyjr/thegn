# Automations

## ADDED Requirements

### Requirement: Automations consume normalized typed events

All rules SHALL consume `AutomationEvent` values with a closed event kind,
stable event ID/key/time, typed optional facts, and optional origin ancestry.
Shipped kinds SHALL cover notification, agent needs-you/finished/failed, PR
checks/review, merge landed, worktree idle, and disk-low facts. Producers MUST
emit at authoritative subsystem chokepoints rather than reconstructing facts by
polling in the evaluator.

#### Scenario: A PR check fact carries its result

- **WHEN** PR hydration observes a checks transition
- **THEN** it emits a `pr_checks` event with the typed check-result field and a
  stable coalescing key

### Requirement: Trusted rules map one event to one catalog action

Ordered global/profile `[[automations.rules]]` entries SHALL use `when`, bounded
AND-combined `if` predicates, and exactly one `then.cap`. Supported actions
SHALL be `sessions.open`, `merge.add`, `notify.push`, and `tools.run`, each with
its closed parameter schema. Event templates MUST NOT select capability IDs,
configured agent/tool names, or shell fragments. Repo-overlay automation tables
MUST be ignored with a surfaced warning.

#### Scenario: Repo persistence is rejected

- **WHEN** `.thegn.toml` declares automation rules
- **THEN** no rule is loaded or executed and the ignored trusted-layer boundary
  is reported

#### Scenario: An unsupported action fails at load

- **WHEN** a rule selects a capability outside the four-action allowlist
- **THEN** config validation rejects the rule before runtime starts

### Requirement: Evaluation is pure, bounded, and auditable

Evaluation SHALL depend only on rules, event, ledger, and injected time. It
SHALL enforce enabled override, debounce, bounded once-per-key state, per-rule
and per-action hourly limits, bounded templates, and origin loop suppression.
Every matched rule SHALL produce either a rendered plan with its exact ledger
transition or an explicit auditable skip reason.

#### Scenario: Origin suppresses recursion

- **WHEN** an event contains automation origin ancestry
- **THEN** a matching rule returns `loop_suppressed` and dispatches nothing

#### Scenario: A once key is durable

- **WHEN** a once-per-key rule has already admitted the same stable event key
- **THEN** subsequent evaluation returns `once_per_key`

### Requirement: Admission and execution are durable and bounded

Plan admission SHALL transactionally persist throttle/once state and the audit
start row. A bounded event queue and bounded concurrency SHALL dispatch plans
through the existing control seam with configured timeouts. Overflow, skips,
success, failure, and timeout SHALL reach terminal audit outcomes; overflow
MUST NOT block the producing subsystem, and failures SHALL emit an
origin-tagged `automation_failed` notification.

#### Scenario: Queue overflow is visible

- **WHEN** a producer submits beyond the bounded runtime queue
- **THEN** it continues without waiting and a `queue_overflow` audit outcome is
  persisted best-effort

### Requirement: Current inspection and pure-test surfaces are cataloged

`automations.list` SHALL report trusted rules, active/inert state, action, and
recent outcome. `automations.test` SHALL evaluate one named rule against a
typed JSON fixture with injected time, returning decisions while opening no
runtime/store and executing no action. Both SHALL be capability-catalog entries
projected to CLI, HTTP, gRPC, and MCP with their declared scopes.

#### Scenario: Test never executes

- **WHEN** a matching fixture is passed to `automations test`
- **THEN** output reports the rendered plan and `executed = false`

### Requirement: Session lifecycle facts use the same envelope

The daemon SHALL produce typed `session_state_changed` and `session_exited`
events from authoritative edges, carrying stable session/worktree/origin facts.
Repeated observations without an edge MUST NOT produce duplicate events, and
both kinds SHALL use ordinary evaluation, admission, and auditing.

#### Scenario: A state edge fires once

- **WHEN** a session changes from working to blocked
- **THEN** one typed state-change event enters the ordinary runtime and a
  repeated blocked observation emits nothing

### Requirement: Scheduled work is an ordinary typed producer

A daemon-hosted scheduled-due producer SHALL emit normalized automation events
through the same runtime. It MUST skip rather than catch up slots missed while
the daemon is down, and MUST skip/audit a due occurrence while the same rule is
still running. It MUST NOT dispatch actions directly or implement a parallel
execution path.

#### Scenario: Daemon downtime has no catch-up storm

- **WHEN** a schedule slot passes while the daemon is stopped
- **THEN** startup emits no retroactive due event

### Requirement: Management surfaces reuse catalog and audit policy

The system SHALL add catalog capabilities and appropriately scoped CLI/control
surfaces to read audited history, persist runtime enable/disable overrides, and
explicitly run or dry-run a named rule. Explicit run MUST use the ordinary
admission/execution/origin/audit path; dry-run MUST execute nothing.

#### Scenario: Runtime disable persists

- **WHEN** an authorized operator disables a rule and the daemon restarts
- **THEN** the persisted override remains effective and is visible in list/
  doctor output

### Requirement: Operational state is diagnosable and tested end to end

Doctor SHALL distinguish active rules, daemon-dependent inert rules, trusted
config sources, and channel/action restrictions. Hermetic end-to-end coverage
SHALL exercise one live action, pure dry-run, repo-layer rejection, loop
suppression, and overflow auditing; generated API/help/config artifacts SHALL
match the final surfaces.

#### Scenario: Daemon-dependent rule is inert visibly

- **WHEN** a daemon-dependent producer is configured but unavailable
- **THEN** doctor/list identifies the rule and reason instead of claiming it is
  active
