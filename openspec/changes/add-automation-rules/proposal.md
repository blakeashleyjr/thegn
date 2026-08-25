# Automations — trigger→action rules off the event bus

Linear: THE-21

## Why

The seed tools this issue catalogs (Superset automations, Codeg automations)
are _schedulers_: cron/RRULE fires an agent session in a fresh workspace.
thegn already has the pieces those tools schedule — the agent-task engine
(`thegn_core::agent_task` + `agent_run.rs`), the notification bus with a rules
engine (`[[notifications.rules]]`), the daemon's edge-triggered session
activity FSM (`blocked · working · done · idle`), the queues' lifecycle
notifications (`queue_landed`, `queue_needs_human`, `pr_queue_merged`, …) and
a capability catalog every door projects. What it does not have is the glue
the roadmap calls **AP 504 "scriptable automations — event-bus triggers →
action-API actions"**: nothing lets the user say _when X happens, do Y_
without sitting at the keyboard.

The uniquely-thegn version of "automations" is therefore **event-triggered,
not schedule-first**: the event bus thegn already maintains (notification
dispatch, session-state edges, session exits, worktree lifecycle) is a richer
trigger vocabulary than a clock, and the actions thegn can already perform
(run a sandbox-wrapped command, notify, hand work to a configured agent,
invoke a catalog verb) are the action vocabulary. A minimal schedule trigger
rides along for the "daily tidy run" case — but thegn is not a scheduler
(judgment in design.md): cron/RRULE/timezone machinery stays deferred with
**Q 226**, and the `automations.run` verb makes systemd timers / cron
first-class drivers of any rule for everything beyond it.

## What Changes

A new **automations** capability: an ordered list of user rules
(`[[automations]]`), each _one trigger → one action_, evaluated by a pure
`thegn_core::automation` module and acted on strictly off the event loop.

- **Triggers** (`on = …`):
  - `notification` — matches at the `notify::record` chokepoint by kind /
    worktree glob / source prefix / message regex / minimum priority (the
    `[[notifications.rules]]` selector vocabulary, reused). Queue events
    (`queue_*`, `pr_queue_*`), agent events (`agent_*`), test/process
    failures and worktree creation are all reachable this way — one selector
    grammar, no per-subsystem trigger list.
  - `session_state` — an edge of the daemon's per-session activity FSM
    (`to = "blocked" | "done" | "idle" | "working"`, optional `from`,
    optional agent/worktree selectors). Edge-triggered, exactly like
    `sessions.wait`.
  - `session_exit` — a session's process ended (optional exit-code selector).
  - `schedule` — minimal built-in tick: `every = "<duration>"` (clamped to a
    floor) or `at = "HH:MM"` + `days` (the DND-window weekday vocabulary).
    Daemon-hosted, no catch-up for missed runs, skip-if-previous-running.
- **Actions** (exactly one per rule, validated at config load):
  - `run` — a command template, spawned off-thread through
    `sandbox_cpucap::wrap_background_argv` (joins the shared `thegn.slice`
    ceilings), cwd = the matched worktree, event fields in
    `THEGN_AUTOMATION_*` env, bounded by a per-rule `timeout`.
  - `notify` — a message template recorded through the same
    `notify::record` chokepoint as every producer (new kind `automation`),
    subject to the user's routing rules — which is how a rule reaches
    desktop/push/chat sinks without knowing about channels.
  - `task` — hand the matched worktree to a configured agent via the shared
    agent-task engine: new `TaskKind::Automation` (generic event vars,
    rule-supplied prompt template, engine quoting contract, watchdog,
    headless `agent_run`).
  - `invoke` — one capability-catalog verb with templated JSON params,
    dispatched through the same catalog door as the control API and admitted
    only when `required_scope(verb)` is within the explicit `[automations]
scopes` ceiling — never a second policy table.
- **Containment (never wedge, never storm).** Evaluation is pure and cheap at
  the chokepoints; actions always execute on a bounded off-thread worker
  (drop-on-overflow with an audit row), capped by `[automations]
max_concurrent`. Per-rule `cooldown` and `max_per_hour` throttles.
  **One-generation guard:** everything an automation causes is tagged
  (`automation:` source prefix, tagged sessions/spawns) and tagged events
  never match rules — a cycle is structurally impossible, not merely
  rate-limited.
- **Per-rule enable / dry-run / audit.** `enabled` and `dry_run` per rule
  (dry-run evaluates, renders the action, records a would-run audit row, acts
  on nothing). Every fire/drop/failure/dry-run lands in a new
  `automation_runs` audit table; runtime enable/disable overrides and
  last-fired stamps persist in `automation_state` (one `user_version` bump).
  Engine/action failures raise a new `automation_failed` notification
  (excluded from triggering) and never propagate further.
- **Surfaces.** CLI namespace `thegn automations list | audit | enable |
disable | run <name> [--dry-run]` — each verb a
  `thegn_core::capability::CATALOG` row gated by `required_scope`, projected
  to HTTP (routes from `ROUTES`), gRPC-or-`SURFACE_GAPS`, and MCP names via
  `CapId::tool_name`. `run` doubles as the test-fire verb and the hook for
  external schedulers. No new panel/zone in v1.
- **Trust boundary (automations are a persistence mechanism).** `[automations]`
  / `[[automations]]` are honored **only from trusted config layers**
  (global/profile). A repo-overlay `.thegn.*` that carries them is ignored
  with a surfaced warning — a cloned repo must never install code that runs
  on events. Aligned with `add-config-trust-resolution`'s constraint class.

## Impact

- **Roadmap:** **AP 504** (scriptable automations — this change's spine);
  **Q 226** (scheduled/cron tasks) deliberately _not_ absorbed — the minimal
  schedule trigger covers its preset tier, the cron/RRULE/timezone tier stays
  deferred and is explicitly out of scope; **AN** gains the audit-log shape as
  a side effect (`automation_runs`).
- **Specs:** new `automations` capability; `state-db` delta (two tables ⇒
  **`user_version` bump** — coordinate the bump number at land time, known
  collision class).
- **Code:** `thegn-core` — `automation.rs` (event model, rule matching,
  throttle/generation logic — pure, table-tested under the 95% gate),
  `config_automations.rs` (validation: exactly-one-action, template
  validation via the agent-task engine's `validate_template`, scope-ceiling
  checks), `TaskKind::Automation` (+ the pinned-count tests every new kind
  trips), db tables + store methods. `thegn-host` — chokepoint taps
  (`notify::record` both in the UI process and the daemon's `notify_push`),
  daemon feed subscriber + schedule ticker (tokio, QoS Background), action
  worker, `cmd/automations.rs`, doctor note, `docs/help/automations.md`.
- **Config:** `[automations]` (`scopes`, `max_concurrent`) +
  `[[automations]]` rule tables — every key documented in
  `config/config.toml.example`.
- **In-flight overlap:** composes with **add-ntfy-push-bridge** /
  **add-chat-webhook-sinks** (a `notify` action reaches phones and chat
  through the notification router — automations add no delivery channel of
  their own); **does not absorb** `add-issue-autopilot` (issue→PR pipeline),
  `add-watched-pr-comment-tasks` (PR comment triggers), or
  `add-agent-orchestration-surface` (supervisor tool surface) — those own
  their domain loops; automations is the generic layer beneath none of them
  (no dependency either way). Uses `add-agent-task-engine` (implemented) for
  the `task` action. `add-config-trust-resolution` shares the trust rationale
  (no code dependency). The in-flight **MCP write-tools scope-gating** branch
  gates how the Write verbs surface over MCP — noted as a dependency, not
  re-scoped. `add-event-feed-subscriptions` improves the feed the daemon
  subscriber rides (no dependency).
- **Event loop:** no new wake source on the UI loop; all action work is
  off-thread with waker-pulsed status. No new chrome; render damage only via
  existing notification surfaces.

## Non-goals

- **A cron/RRULE/timezone scheduler** (Q 226's full tier). External
  schedulers call `thegn automations run <name>`.
- **Repo-supplied automations**, under any prompt or TOFU flow, in v1.
- **A visual rule builder / panel section.** CLI + config + audit first.
- **Chained or multi-action rules, conditions on action results.** One
  trigger, one action; compose via the event bus if genuinely needed.
- **Windows-event or filesystem-watch triggers.** The trigger vocabulary is
  thegn's own event bus.
