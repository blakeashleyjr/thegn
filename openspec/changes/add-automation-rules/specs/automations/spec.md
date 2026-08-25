# Automations

## ADDED Requirements

### Requirement: Automation rules bind one trigger to one action

The system SHALL evaluate an ordered list of user automation rules
(`[[automations]]`), each binding exactly one trigger to exactly one action.
A rule declares its trigger via `on = "notification" | "session_state" |
"session_exit" | "schedule"` plus trigger-specific selectors, and exactly one
action (`run` command template, `notify` message template, `agent`+`prompt`
task handoff, or `invoke`+`params` capability call). Config validation MUST
reject a rule with zero or multiple actions, a duplicate `name`, a template
referencing an unknown variable, or a shell-quoted placeholder in a command
template — at load, never as a silent no-op at fire time. Rule evaluation
MUST be a pure `thegn-core` function over a normalized event, and no
automation work may block the render loop: evaluation happens at existing
off-loop chokepoints and every action executes on a bounded off-thread
worker.

#### Scenario: A queue event triggers a command

- **WHEN** a rule with `on = "notification"`, `kind = "queue_needs_human"`
  and `run = "notify-send 'queue stuck'"` matches a recorded notification
- **THEN** the command is spawned off-thread in the matched worktree,
  sandbox-wrapped via the shared background ceilings, and the render loop
  receives no new wake source from the evaluation

#### Scenario: An invalid rule fails at load

- **WHEN** a rule declares both `run` and `notify`
- **THEN** config validation reports the rule by name as an error and no
  automation runs

### Requirement: Notification triggers reuse the notification selector vocabulary

Rules with `on = "notification"` SHALL match at the notification dispatch
chokepoint using the `[[notifications.rules]]` selector vocabulary (kind,
worktree glob, source prefix, message regex, minimum priority), and MUST
evaluate after the user's notification rules so a dropped notification is
invisible to automations. Queue, PR-queue, agent, test, process, and worktree
lifecycle events SHALL be reachable through their existing notification kinds
with no parallel trigger plumbing.

#### Scenario: Automations see what the inbox sees

- **WHEN** a `[[notifications.rules]]` entry drops a notification with
  `drop = true`
- **THEN** no `on = "notification"` automation fires for it

#### Scenario: Worktree lifecycle triggers ride the kind

- **WHEN** a `worktree_created` notification is recorded and a rule selects
  `kind = "worktree_created"` with a matching worktree glob
- **THEN** the rule's action is planned

### Requirement: Session triggers are edge-triggered daemon events

Rules with `on = "session_state"` SHALL fire on edges of the daemon's
per-session activity state (`working`, `blocked`, `done`, `idle` — the same
vocabulary as `sessions.wait --until`), selectable by target state, optional
source state, and agent/worktree selectors; a re-broadcast of an unchanged
state MUST NOT fire. Rules with `on = "session_exit"` SHALL fire when a
session's process terminates, with an optional exit-code selector. Both
trigger classes evaluate in the daemon process; when the daemon is disabled
they are inert and `thegn doctor` and `thegn automations list` MUST say so.

#### Scenario: Blocked agent pings once

- **WHEN** a session transitions `working → blocked` and a rule selects
  `to = "blocked"` with a `notify` action
- **THEN** the notification is recorded once, and repeated blocked
  observations without an intervening state change fire nothing

#### Scenario: Daemonless is honest

- **WHEN** `[daemon] enabled = false` and a `session_state` rule is
  configured
- **THEN** `thegn automations list` marks the rule inert with the reason

### Requirement: Schedule triggers are a bounded tick, not a scheduler

Rules with `on = "schedule"` SHALL support `every = "<duration>"` (clamped to
a minimum of 60 seconds) or `at = "HH:MM"` with optional `days` weekday
tokens. Schedule evaluation runs in the daemon; a slot occurring while the
daemon is not running MUST be skipped, never caught up; a due tick while the
rule's previous action is still running MUST be skipped and audited; and
last-fired state MUST persist so restarts neither re-fire nor storm. Cron,
RRULE, and timezone-rule scheduling are out of scope — `thegn automations
run <name>` is the supported hook for external schedulers.

#### Scenario: Missed slots are skipped visibly

- **WHEN** a rule with `at = "09:00"` exists and the daemon was down from
  08:00 to 10:00
- **THEN** the rule does not fire at daemon start, and the audit log shows no
  run for that day

#### Scenario: Overlap is skipped

- **WHEN** an `every = "5m"` rule's action is still running at the next tick
- **THEN** the tick is skipped and recorded as skipped in the audit log

### Requirement: Actions are contained and composed from existing machinery

`run` actions SHALL spawn through the shared background sandbox wrap (joining
the `[sandbox.limits]` slice), with the matched worktree as cwd, event fields
in `THEGN_AUTOMATION_*` env, and a per-rule timeout. `notify` actions SHALL
record through the same notification chokepoint as every producer, subject to
the user's routing rules. `agent`+`prompt` actions SHALL dispatch through the
shared agent-task engine as a headless run with its quoting contract and
watchdog. `invoke` actions SHALL dispatch exactly one capability-catalog verb
through the same catalog door as the control API, admitted only when the
verb's `required_scope` is within the explicit `[automations] scopes`
ceiling — never a second policy table; an `invoke` outside the ceiling is a
config-load error.

#### Scenario: Invoke respects the scope ceiling

- **WHEN** `[automations] scopes = ["read"]` and a rule configures `invoke`
  on a Write-scoped verb
- **THEN** config validation rejects the rule by name at load

#### Scenario: Notify action reaches configured channels

- **WHEN** a rule's `notify` action fires and the user's notification rules
  route that kind to push
- **THEN** the message is delivered through the notification router like any
  producer's, with no automation-specific delivery code

### Requirement: Automations never storm and never cascade

Events caused by an automation action SHALL be tagged (an `automation:`
source prefix on recorded notifications; an origin marker on spawned
processes and sessions), and tagged events MUST match no rule, making rule
cycles structurally impossible. Each rule SHALL enforce a `cooldown` (default
30s) and `max_per_hour` (default 30), with throttled fires dropped and
audited. The action worker SHALL be bounded (global `max_concurrent`, bounded
queue) with overflow dropped and audited, and an action failure or panic MUST
be contained to an audit row plus an `automation_failed` notification —
which, being tagged, cannot itself trigger a rule.

#### Scenario: A rule cannot trigger itself

- **WHEN** a rule's `notify` action records a notification whose kind and
  worktree would match the same rule
- **THEN** the recorded notification carries the `automation:` source and
  fires no rule

#### Scenario: A failing action never wedges the engine

- **WHEN** a `run` action exits non-zero or exceeds its timeout
- **THEN** an audit row records the failure, an `automation_failed`
  notification is raised, and subsequent events keep evaluating normally

### Requirement: Per-rule enable, dry-run, and a durable audit trail

Each rule SHALL honor `enabled` (default true) and `dry_run` (default false):
a dry-run rule evaluates, renders its action, records a would-run audit row,
and performs nothing. Every fire, drop, skip, failure, and dry-run SHALL be
recorded in a durable `automation_runs` audit log (rule, trigger, rendered
action, outcome, timestamps) with bounded retention, and runtime
enable/disable overrides SHALL persist across restarts.

#### Scenario: Dry-run shows without doing

- **WHEN** a matching event occurs for a rule with `dry_run = true` and a
  `run` action
- **THEN** the audit log records the rendered command as a would-run entry
  and no process is spawned

#### Scenario: A runtime disable survives restart

- **WHEN** `thegn automations disable <name>` is issued and thegn restarts
- **THEN** the rule remains disabled until explicitly re-enabled

### Requirement: Repo-supplied automations never run

`[automations]` and `[[automations]]` SHALL be honored only from trusted
config layers (global and profile). A repo-overlay config carrying automation
tables MUST have them ignored with a surfaced warning naming the file — a
cloned repository must never install rules that execute on events, with no
prompt-through or trust-on-first-use path.

#### Scenario: A hostile repo config is inert

- **WHEN** a checked-in `.thegn.toml` contains a `[[automations]]` rule with
  a `run` action
- **THEN** the rule is stripped from the effective config, a warning is
  surfaced, and no event ever fires it

### Requirement: Automation surfaces are capability-catalog rows

The automation surfaces — list rules with status, read the audit log, set a
rule's enabled override, and fire a rule manually (`run <name>
[--dry-run]`) — SHALL each be a `thegn_core::capability::CATALOG` row gated
by `required_scope`, projected to the CLI (`thegn automations …`), HTTP via
the `ROUTES` table, gRPC or a `SURFACE_GAPS` entry, and MCP via
`CapId::tool_name`. The manual fire verb MUST execute the rule's action
through the same worker and audit path as an event-triggered fire.

#### Scenario: Manual fire is a first-class trigger

- **WHEN** an authorized client calls the automation run verb for a named
  rule
- **THEN** the rule's action executes through the same containment and audit
  path as an event fire, enabling external schedulers to drive rules

#### Scenario: Scope gates the write verbs

- **WHEN** a token lacking the write scope calls the set-enabled verb
- **THEN** the request is rejected without changing the rule
