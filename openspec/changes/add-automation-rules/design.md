# Design — automation rules

## Shape of the thing

Three layers, matching the repo's producer/consumer pattern:

1. **Pure core** (`thegn_core::automation`): a normalized `AutomationEvent`
   (`Notification { kind, source, message, worktree, priority }`,
   `SessionState { session, agent, worktree, from, to }`,
   `SessionExit { session, code }`, `ScheduleTick { rule, at }`), a `Rule`
   parsed from config, and
   `evaluate(rules, event, state, now) -> Vec<Planned>` where `state` is the
   throttle ledger (last-fired, hourly counts, enabled overrides) passed in
   as data. Also pure: template rendering for `notify`/`run`/`task`/`invoke`
   payloads (reusing `agent_task`'s substitution + `validate_template`
   quoting contract), the one-generation tag check, and schedule due-ness
   (`due(rule, last_fired, now)`). All of it carries the 95% line gate.
2. **Chokepoint taps** (thegn-host): each event class is observed at exactly
   one place per process, which is what makes double-fire impossible:
   - `notification` rules evaluate inside `notify::record` (UI process) and
     the daemon's `notify_push` store path — whichever process records the
     row evaluates it, and a row is recorded once.
   - `session_state` / `session_exit` rules evaluate in the **daemon**, on
     the same edge-triggered broadcast that feeds `sessions.wait`.
   - `schedule` rules tick in the **daemon** (a tokio interval task, QoS
     Background). No daemon ⇒ schedule and session triggers are inert;
     `thegn doctor` and `automations list` say so rather than pretending.
3. **Action worker** (thegn-host): a bounded mpsc of `Planned` actions
   drained by a small worker pool (cap `[automations] max_concurrent`,
   default 2; queue bound ~32, overflow ⇒ drop + audit row). Every completion
   or failure writes `automation_runs` and, on failure, records an
   `automation_failed` notification. Status reaching the UI rides the
   existing channel + `TerminalWaker` pulse pattern.

Evaluation at a chokepoint is a pure function over a small rule list — a few
string/glob/regex matches (regexes precompiled at config load) — so it is
allowed inline where the event already is; anything that does I/O is on the
worker. **The render loop never runs an action and gains no wake source.**

## Rule config

```toml
[automations]
scopes = ["read"]          # ceiling for `invoke` actions; empty = invoke disabled
max_concurrent = 2

[[automations]]
name    = "ping-on-needs-human"     # unique; validated
enabled = true                       # default true
dry_run = false                      # default false
on      = "notification"
kind    = "queue_needs_human"        # + worktree / source / message / min_priority
cooldown = "60s"                     # default 30s
max_per_hour = 30                    # default 30
notify  = "queue needs a human: {message}"

[[automations]]
name = "tidy-daily"
on   = "schedule"
at   = "09:00"
days = ["mon", "tue", "wed", "thu", "fri"]
run  = "just tidy"                   # sandbox-wrapped, timeout-bounded
timeout = "10m"
```

Exactly one of `run` / `notify` / (`agent` + `prompt`) / (`invoke` +
`params`) per rule — zero or several is a config error, as is a template
referencing an unknown variable, a quoted placeholder in a command template,
or an `invoke` whose `required_scope` is not covered by `[automations]
scopes` (loud at load, never a silent no-op at fire time).

## Trigger sources, concretely

- The notification tap sees every kind in `NotificationKind::ALL` plus
  CLI/API-pushed notes — so merge-queue (`queue_landed`, `queue_ready`,
  `queue_needs_human`), PR-queue (`pr_queue_merged`, …), agent
  (`agent_done`/`agent_failed`/`agent_attention`), tests, process exits and
  `worktree_created` need no bespoke trigger plumbing. It taps **after** the
  user's `[[notifications.rules]]`: a rule that `drop`s a notification also
  hides it from automations (one mental model: automations see what the
  inbox sees).
- Session FSM edges reuse the daemon's existing edge-triggered transition
  broadcast (`daemon/session.rs`); the trigger vocabulary is exactly the
  `wait --until` word set.
- Worktree lifecycle rides `worktree_created` (and future lifecycle kinds)
  through the notification tap rather than a parallel event type.

## Is thegn a scheduler? (judgment)

No. Superset and Codeg are scheduler-first because scheduling is their only
trigger; thegn's daemon is a long-lived process that can _tick_, which is a
different promise:

- Fires only while the daemon runs; a missed slot is skipped, never caught
  up (`automation_runs` shows the gap — visible, not silent).
- `every` is clamped to a 60s floor; `at` fires once per matching day.
- One in-flight run per rule; a due tick while running is skipped and
  audited (Codeg's semantics, honestly the only safe ones).

That covers the preset tier of Q 226 (hourly/daily/weekdays). The cron/
RRULE/IANA-timezone tier stays deferred: `thegn automations run <name>` is a
catalog verb, so systemd timers or cron drive any rule with real scheduler
semantics the moment someone needs them — building a worse cron inside thegn
buys nothing.

## Storm and loop containment

- **One-generation guard.** Actions stamp their consequences: `notify`
  actions record with source `automation:<rule>`; `task`/`run` spawns carry a
  `THEGN_AUTOMATION_ORIGIN` marker the taps check (agent-completion
  notifications for an automation-launched session carry the tagged source).
  A tagged event matches no rule. This is the invariant that makes
  rule-cycles structurally impossible; cooldown/rate caps are defense in
  depth, not the primary guard.
- Per-rule `cooldown` + `max_per_hour`; drops are audited so a throttled
  rule is debuggable.
- Global `max_concurrent` + bounded queue; overflow drops (audited) rather
  than backing up into the chokepoint.
- A panicking or erroring action is contained by the worker: audit row +
  `automation_failed` notification, engine keeps running. The taps never
  propagate an error into `notify::record`'s caller.

## Security

- **Persistence mechanism, trusted layers only.** An automation is "run this
  when that happens, forever" — exactly what malware wants from a cloned
  repo. `[automations]`/`[[automations]]` are read from global and profile
  layers only; a repo overlay carrying them is ignored with a surfaced
  warning (statusbar + `thegn config explain` once
  `add-config-trust-resolution` lands; a plain warning until then). No TOFU
  path in v1 — repo-supplied rules never auto-run, full stop.
- **`run` is arbitrary code by design** — but only user-authored, and every
  spawn goes through `wrap_background_argv` (shared `thegn.slice`
  CPU/memory ceilings, same fail-safe rules as the fold gate) with a
  timeout. Command templates are shell-quoted per the agent-task engine's
  contract; the prompt for `task` actions is never shell-quoted (verbatim
  env), same as every queue handoff.
- **`invoke` is catalog-only** and double-gated: the verb's
  `required_scope` must be within the explicit `[automations] scopes`
  ceiling (default `["read"]`; empty disables invoke). No shell, no second
  policy table, no admin bypass. This mirrors the ntfy command-inbox
  admission rule — one pattern for "machine-initiated capability calls".
- **Secrets:** no new credential class. Anything needing a token does so via
  the invoked subsystem's existing SecretRef config (e.g. chat sinks).
- **Blast radius:** the audit table records every fire with its rendered
  action, so post-incident "what did automations do" is one query.
  `automations disable` / the persisted override is the kill switch and
  survives restart.

## SQLite

Two new tables, one `user_version` bump (number chosen at land time —
`SCHEMA_VERSION` collisions with sibling changes are a known merge hazard):

- `automation_runs(id, rule, trigger_kind, event_summary, action_kind,
rendered, outcome, detail, started_at, finished_at)` — bounded by a
  retention sweep (keep last N per rule).
- `automation_state(rule PRIMARY KEY, enabled_override, last_fired_at,
hour_count, hour_window_start)` — throttle ledger + runtime toggle, shared
  by both evaluating processes (best-effort read-modify-write; a rare racing
  double-fire across processes is bounded by cooldown and acceptable).

## Render / help / catalog gates

- Render damage: none new. `notify` actions surface through existing
  notification chrome (toast ⇒ existing `Full` path). No new zone or panel,
  so no `zone:*`/`panel:*` help context; the new CLI action ids are claimed
  by `docs/help/automations.md` (help + prose ratchets).
- Catalog: `automations.list` (Read), `automations.audit` (Read),
  `automations.set_enabled` (Write), `automations.run` (Write) — Verb enum +
  `required_scope` + `CATALOG` rows + `ROUTES`; gRPC mirrored or a
  `SURFACE_GAPS` entry; MCP tool names fall out of `CapId::tool_name` (Write
  tools ride the in-flight MCP scope-gating branch's `--scopes` mechanism).
- New `NotificationKind::Automation` + `NotificationKind::AutomationFailed`
  trip the pinned kind-list/example-prose tests — updated in the same change.

## Alternatives considered

- **A separate trigger type per subsystem** (queue triggers, agent triggers,
  …): rejected — the notification bus already normalizes those domains; one
  selector grammar beats N.
- **Engine in the UI process only**: rejected — dies with detach, which
  breaks the headline "act while I'm away" case; the daemon is the natural
  home (same reasoning as the ntfy inbox).
- **Engine in the daemon only**: rejected — UI-process notifications would
  need forwarding before evaluation; the per-chokepoint split is smaller and
  has no double-fire mode.
- **Cron/RRULE in-process**: rejected (judgment above).
- **Actions as plugin hooks**: the plugin runtime may later _add_ action
  kinds; the four built-ins don't wait for it.

## Open questions

- Should `dry_run` also be a global mode (`[automations] dry_run = true`)
  for first-time bring-up? Cheap; leaning yes at implementation time.
- Retention default for `automation_runs` (last 200 per rule?).
- Whether `session_state` triggers should offer a `for = "<duration>"`
  debounce (blocked-for-2m) in v1 or wait for demand.
