# THE-21 — Automations architecture

Status: implementation design for `tg/the-21-automations`.

## Decision

Add a trusted, typed rule engine with the shape `when event [if fields] then
catalog action`. Matching is a pure `thegn-core` operation. A host-owned runtime
receives normalized events, persists throttle/audit state, and dispatches bounded
work off the compositor loop. The runtime reports outcomes through the existing
notification and Monitor paths.

The rule engine is deliberately not a second plugin system, scheduler, command
runner, or notification bus. An action is a catalog capability id plus a
validated, name-only parameter object. The v1 action set is:

- `sessions.open`, with `AgentLaunch.prompt` populated from the event and the
  configured agent/task template;
- `merge.add`;
- `notify.push`; and
- a new cataloged `tools.run` adapter for a named trusted `[[tools]]` entry.

`tools.run` is necessary because the current catalog has `launch.preset`, not a
capability that means “run one named `[[tools]]` command”. It must be added to
the one catalog and all its policy/surface tests; it must not become a private
`Command::new` path in the automation worker. `pins.mark` and
`lifecycle.hook` are not v1 actions: this branch has configured pins and
lifecycle policy, but no corresponding catalog capabilities. Accepting either
would violate the catalog invariant. The action type leaves the seam open for a
future catalog row. Webhook delivery is likewise not part of THE-21; a future
THE-62 sink can subscribe to the standard notification delivery seam.

## Audit of the existing branch

The openspec draft is useful for the vocabulary and scenarios, but several of
its implementation claims are stale or too broad.

Already present and to reuse:

- `Notification`, the 26 built-in kinds, priorities, and the notification
  attention vocabulary are in `crates/thegn-core/src/notification.rs:10-110`.
  `notification_route::decide` is pure and applies ordered selectors, drop,
  channel routing, DND, and priority at `crates/thegn-core/src/notification_route.rs:73-186`.
- Daemon activity transitions are already edge-triggered. `SessionActor::publish_state`
  refuses unchanged state/error edges at
  `crates/thegn-host/src/daemon/session.rs:784-819`; the wire event carries
  worktree, state, activity, timestamp, message, and THE-89 `error_active` at
  `crates/thegn-svc/src/control/mod.rs:226-263`.
- THE-89 is already the source of the agent error signal: the actor loads
  `cfg.notifications.agent_error_signatures` and publishes the separate
  `error_active` bit. Automation must consume that edge, not reimplement text
  classification.
- `OpenSpec.agent` is the existing configured-agent launch seam. It resolves
  the agent, prompt, sandbox, environment, worktree binding, and headless
  mode in `crates/thegn-svc/src/control/mod.rs:109-205`. `agent_task` already
  owns pure prompt/command template rules, while `agent_run` owns the bounded
  off-loop process runner (`crates/thegn-core/src/agent_task.rs:29-113`,
  `crates/thegn-host/src/agent_run.rs:104-220`).
- `sessions.open`, `merge.add`, `notify.push`, and `launch.preset` already have
  catalog rows in `crates/thegn-core/src/capability.rs:183-370`, with one
  verb-to-scope table in `crates/thegn-core/src/control.rs:237-398`.
- Config precedence is defaults → file → profile → env → `--set` in
  `crates/thegn-core/src/config.rs:6076-6139`. DB schema changes are additive
  through `db_migrate::additive_schema`; the current version is 61
  (`crates/thegn-core/src/db.rs:130-142`, `crates/thegn-core/src/db.rs:920-995`).

Claims to correct:

1. `host::notify::record` is not a universal notification tap. It routes and
   records only callers that opt into it (`crates/thegn-host/src/notify.rs:284-332`).
   Hydration, queue, disk, plugin, CLI, and daemon paths call
   `NotificationStore::put_notification` directly; the daemon `notify.push`
   implementation does so at `crates/thegn-host/src/daemon/service.rs:1118-1145`.
   THE-21 must first introduce one notification emission helper and migrate
   those producers. Adding a UI tap and a daemon tap would duplicate fires.
2. `agent_attention` is live state by default, not an ordinary inbox event.
   `SessionActor::on_attention` writes `session_attention` and only writes an
   inbox row when `[notifications].agent_attention_inbox` is enabled
   (`crates/thegn-host/src/daemon/session.rs:822-917`). “Agent needs you” must
   therefore be the blocked/attention transition, not a forced inbox row.
3. Current PR notifications use the generic `pr_state_changed` kind and queue
   kinds; there are no distinct checks-passed, checks-failed, or review-requested
   enum variants (`crates/thegn-core/src/notification.rs:51-81`). Preserve this
   stable kind set in v1. Where a producer has the richer PR fact, put it in the
   normalized event fields; do not infer it from message text or multiply enum
   variants without a producer and ratchet story.
4. Current daemon “idle” is an interactive attach/lease transition, not a
   worktree-idle timer (`crates/thegn-host/src/daemon/session.rs:655-680`). A
   worktree-idle rule needs an event-derived deadline, armed only when an active
   rule requires it, and run in the daemon/host worker. It must never add a
   compositor ticker.
5. The draft’s `TaskKind::Automation` is not required. `TaskKind` currently
   selects six queue/issue prompt contracts and has pinned exhaustive counts
   (`crates/thegn-core/src/agent_task.rs:29-113`, `:543-551`, `:1539-1543`).
   Render an automation prompt with a bounded event variable map using the
   existing template parser, then call the cataloged `sessions.open`/
   `OpenSpec.agent` seam. Add a task kind only if an implementation proves that
   the existing prompt/launch contract cannot carry this data; do not create a
   fake queue semantic just to make the name fit.

## Core contract

Create `crates/thegn-core/src/automation.rs` as a substrate-free module. It
contains `AutomationEvent`, `AutomationRule`/predicate types, `PlannedAction`,
`EventKey`, and `evaluate(rules, event, state, now)`. It imports no DB, tokio,
filesystem, process, terminal, or provider type.

The normalized event has stable fields: event id/time, kind, workspace/repo,
worktree, branch, agent role, priority, source reference, message, session id,
PR facts when known, and an origin marker. The event kinds are projections of
existing facts: notification, agent-needs-you, agent-finished, agent-failed,
PR-checks, PR-review-requested, merge-landed, worktree-idle, and disk-low.
Missing fields never match. All configured predicates are ANDed; branch matching
uses the repository’s glob implementation, with invalid globs rejected during
config validation.

`evaluate` returns a decision for each matching enabled rule in config order. It
also returns explicit skip decisions for disabled, debounced, once-per-key,
rate-limited, unsupported-action, and loop-suppressed cases so the host can
audit them without re-running policy. State is keyed by stable rule id plus
event key, not by a message or wall-clock string. The pure tests must cover:

- every event predicate and missing-field behavior;
- debounce and once-per-key across injected timestamps;
- bounded sliding-window rule/action rate limits;
- deterministic ordering;
- action parameter rendering and validation; and
- origin suppression: v1 automation-originated events are visible but are not
  eligible for any automation, so an action cannot cascade into itself or a
  second rule.

The origin contains root event id, rule id, and run id. It is carried through
notification emission and `OpenSpec`/session activity. It is internal metadata,
not a user-supplied action parameter. A failed or dropped event cannot lose its
audit row merely because its action was not executed.

## Config and trust

Add `crates/thegn-core/src/config_automations.rs`, re-export it from `lib.rs`,
and add bounded automation settings plus rules to `Config` with a safe
disabled/empty default. The openspec draft shows both `[automations]` and
`[[automations]]` at the same TOML path, which is invalid TOML. Keep the
requested array-of-rules concept with a valid `[automations]` settings table and
`[[automations.rules]]` entries. The rule syntax should read directly as the
lead framing, for example:

```toml
[automations]
enabled = true
max_concurrent = 2
queue_capacity = 64

[[automations.rules]]
name = "tell-me-when-a-coder-is-blocked"
enabled = true
when = "agent_needs_you"
debounce_secs = 60
once_per_key = true

[automations.rules.if]
workspace = "~/code/product"
branch = "tg/*"
agent_role = "coder"
priority = "alert"

[automations.rules.then]
cap = "notify.push"
title = "Coder needs attention"
body = "{message}"
urgency = "alert"
```

This valid nesting is the implementation shape; it still gives users one
`[[automations.rules]]` entry per `when … then …` rule. It must express exactly
one `when` and one catalog `then.cap`. Config
validation rejects duplicate/empty names, oversized prompts/fields, invalid
globs, unknown event kinds, unsupported catalog ids, unbounded rate/timeout
values, and action parameters that are not name-only where required. Validate
templates using the existing `agent_task` parser; never shell-expand event data.

Automations are honored only from the global file and the trusted profile
overlay. Do not add them to `RepoConfigFile`. Add a named warning when a repo
`.thegn.toml/.yaml/.json` contains `[automations]` or `[[automations]]`, using
the same trust rationale as the command-collector rejection at
`crates/thegn-core/src/config.rs:4401-4419` and the effective notification
overlay at `:6527-6560`. The repo’s rule text must never be parsed into the
active runtime, even if the repo is trusted for other features.

Document every key in `config/config.toml.example` beside `[notifications]`
and add `docs/help/automations.md`, with the config trust rule and dry-run CLI
documented. Keep environment overrides deliberately absent for the rule list
and execution actions; pin each new shallow key with a reason in
`test/env-overlay-ratchet.txt` (or add a narrowly justified scalar knob to
`Config::env_overlay`). No automation setting may silently become an env-based
code-execution door.

## Events and host runtime

Create small host modules rather than extending `run.rs`, `notify.rs`, or
`daemon/service.rs` into god files:

- `automation_events.rs` owns the normalized event envelope and the one
  notification emission adapter. It calls `notification_route::decide` once,
  records only when the route permits inbox visibility, and submits the same
  event to the runtime. The event carries the final effective priority.
- `automation_runtime.rs` owns the per-process subscription, in-memory
  coalescing, and the event-derived schedule/deadline. It has no subprocess
  code. Daemon session edges enter here from the already-broadcast
  `SessionActivityEvent`; `SessionExit` enters at the daemon service edge.
- `automation_executor.rs` owns the bounded tokio worker, semaphore, per-action
  deadline, audit transitions, and outcome notification. SQLite work is on
  `spawn_blocking`; action execution is never on the render loop.

Migrate every current notification producer to the one adapter, preserving its
existing `put_notification` versus `put_notification_once` semantics: the
hydration tracker/feed, hydrate PR/issue diff, disk scan, test result, worktree
creation, queue/merge handlers, plugin/provision/repo-trust handlers, CLI
notify push, and daemon `notify_push`. The adapter is the only place that
routes, records, and emits an automation event. A producer that cannot provide
rich PR facts still emits its current kind and fields.

The event mapping is explicit:

| Requested event         | Branch source                                                                       | v1 rule                          |
| ----------------------- | ----------------------------------------------------------------------------------- | -------------------------------- |
| agent needs you         | daemon blocked/attention edge; live `session_attention` remains the source of truth | supported                        |
| agent finished          | activity `done` / session exit plus existing `agent_done` fact                      | supported                        |
| agent failed            | THE-89 `error_active` edge and existing `agent_failed` producer                     | supported                        |
| PR checks/review/merge  | existing PR/queue producers; richer fields only when present                        | supported without new enum kinds |
| worktree idle N minutes | last activity / session edge, daemon deadline                                       | supported; no UI poll            |
| disk low                | existing disk/stat alert result                                                     | supported; no second scan        |

If a producer cannot distinguish “checks passed” from generic `pr_state_changed`,
`thegn automations test` must report that fact rather than claim a match from
message text. The runtime does not invent forge truth; git/forge/provider seams
remain the source of truth.

For zero idle wakeups, the compositor continues to use
`poll_input(None)` when idle. The runtime is a daemon/worker concern. A schedule
deadline is computed from rule config and armed only if there is a schedule or
idle rule; the minimum interval is bounded (60 seconds), and a worker wake is
not a terminal/render wake. Event channels are bounded; overflow is dropped with
an audit/log record, never allowed to backpressure PTY or rendering.

## Action execution and visibility

Before execution, validate the planned catalog id and required scope against the
automation policy. Dispatch through the same catalog/control implementation:

- `sessions.open` receives a worktree-scoped `OpenSpec.agent` and a rendered
  bounded prompt. It inherits configured agent/sandbox/resource limits and does
  not accept arbitrary argv from the event.
- `merge.add` receives only the event’s resolved worktree and reuses
  `merge_ops`/the control API behavior.
- `tools.run` resolves a name against trusted global/profile `[[tools]]`, then
  uses the existing configured-command/sandbox/CPU-cap runner. No event field
  becomes a shell fragment or executable path.
- `notify.push` goes through the canonical notification emitter. Add
  `automation` and `automation_failed` notification kinds so Monitor/inbox
  status is truthful; update `NotificationKind::ALL`, priority, glyph/label,
  config help, and their count/snapshot tests. The generated notification is
  origin-tagged and therefore visible but not an automation input.

Every run gets an audit row before dispatch and a terminal row for fired,
skipped, dropped, succeeded, timed out, or failed. Persist bounded summaries,
not unbounded rendered prompts or secrets. Add `automation_runs` and
`automation_state` in a v62 additive migration, with indexes for rule/time and
bounded retention. The DB is cache/audit state; action truth remains in the
existing catalog/provider subsystems.

## CLI, catalog, and ratchets

Add `thegn automations list` and `thegn automations test` in a new
`crates/thegn-host/src/cmd/automations.rs` module. `list` prints enabled,
trusted-layer, event/action, inert reason, and recent outcome summary. `test`
takes a named rule plus a JSON/event fixture (or equivalent typed flags), runs
only core `evaluate` with an injected timestamp, and prints planned/skipped
actions; it never executes an action and never writes the live DB. Both support
the repo’s JSON output convention.

Add catalog rows `automations.list` (Read) and `automations.test` (Read) and
route them through the normal control spine. Because the rows are read-only and
pure/config-backed, implement HTTP/API_CALLS, gRPC, MCP state, and generic
plugin dispatch together; do not add a special automation transport. Update
`Verb`, `required_scope`, `Verb::ALL`, `CATALOG`, `ROUTES`, `API_CALLS`,
`GRPC_CAPS`, `MCP_STATE_CAPS`, control wire schemas, and the scope tests. The
action ids inside rules are resolved against the same catalog; no second action
registry is allowed.

Update `cli_help::GROUPS`, completions, `docs/help/automations.md`, and the
relevant help/config prose ratchets. Run the env-overlay and completion-slot
ratchets after the config/CLI shape is final. Regenerate
`docs/api/control-v1.json` only through the documented snapshot command; do not
hand-edit it. No panel context is introduced, so `help-context-ratchet.txt`
must not gain a fake panel key.

## Cut list and acceptance gates

Cut from the openspec draft for this branch:

- UI and daemon “taps” as separate sources; replaced by one producer adapter.
- `TaskKind::Automation`; existing `OpenSpec.agent` plus the pure template
  parser is enough.
- `automations.run`, enable/disable, cron/RRULE/time-zone scheduling, and a
  generic `invoke` escape hatch; the issue asks for `list/test`, and all action
  execution must remain cataloged and trusted.
- repo automations, webhook sinks, pins.mark, lifecycle.hook, and new provider
  implementations.

Required focused verification: `just quick thegn-core`, `just quick thegn-svc`,
and `just quick thegn-host`; focused `cargo nextest run -p thegn-core
automation`, `-p thegn-svc control_schema`, and `-p thegn-host automations` (with
the actual test filters adjusted to the final names). Do not run `just test`,
`just ci`, full-workspace compiles, or e2e. Any CLI invocation during testing
must set `XDG_STATE_HOME` to a fresh temporary directory; do not migrate or run
the built binary against live state.
