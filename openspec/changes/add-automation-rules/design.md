# Design — typed events to catalog actions

## Event envelope

`AutomationEvent` is the only trigger input. It carries a closed event kind,
stable ID/key/time, optional typed facts, and optional `AutomationOrigin`.
Producers normalize existing notification, daemon activity, PR/merge, idle,
and disk facts at authoritative chokepoints. Future session-state, session-exit,
and schedule support must add ordinary event kinds/producers; it may not create
a second scheduler-to-executor path.

## Trusted rules and closed actions

Only global and profile layers contribute rules. A repo overlay with an
`automations` key is ignored with a warning. Each rule names one event in
`when`, AND-combines bounded `if` predicates, and names one supported catalog
capability in `then.cap`.

Action parameters are a closed schema:

| Capability      | Parameters                                               |
| --------------- | -------------------------------------------------------- |
| `sessions.open` | required configured `agent`, optional templated `prompt` |
| `merge.add`     | event worktree only; no configured params                |
| `notify.push`   | required templated `body`, optional `title`/`urgency`    |
| `tools.run`     | required configured tool `name`                          |

Event values can fill bounded templates but cannot select the capability or a
configured executable name. This retains the capability catalog and configured
tool/agent lists as the authorization boundary.

## Evaluation, ancestry, and admission

Core evaluation depends only on rules, event, ledger, and injected time. A
matching rule either yields a rendered plan plus its exact state transition or
an auditable skip: disabled, debounced, once-per-key, rule/action rate-limited,
unsupported action, loop-suppressed, or invalid template.

Every dispatched action carries root-event/rule/run ancestry through control
requests and durable handoffs. Caused notifications remain visible but their
origin suppresses all further rule matches. The DB admits plans and persists
rate/once state plus initial audit rows transactionally, preventing duplicate
execution across racing processes.

## Runtime

A bounded channel feeds a dedicated runtime with bounded concurrency. Overflow
creates a durable queue audit without blocking the producer. Actions dispatch
through the existing control client; configured tools are timeout-bounded and
killed on expiry. Terminal outcome updates the audit, bounded retention runs,
and failures emit origin-tagged `automation_failed` notifications.

## Surfaces

Shipped `list` and pure `test` are catalog-backed and projected through CLI,
HTTP, gRPC, and MCP. `test` opens no runtime/store and executes nothing.
History, enable/disable, and explicit run/dry-run remain future catalog
capabilities; they must reuse the same admission, execution, origin, and audit
paths. Doctor must separately report config/trust and daemon-dependent inert
rules.

## Schedule judgment

Scheduled-due is a producer, not a second automation engine. It emits a typed
event only while the daemon is running, does not catch up missed slots, and
audits a due event skipped because the same rule is already executing. General
cron/RRULE/timezone orchestration remains external.
