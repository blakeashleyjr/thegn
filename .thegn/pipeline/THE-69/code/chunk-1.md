---
files:
  - crates/thegn-core/src/budget_alert.rs
  - crates/thegn-core/src/lib.rs
  - crates/thegn-proxy/src/budget.rs
  - crates/thegn-host/src/usage_budget.rs
  - crates/thegn-host/src/main.rs
  - crates/thegn-host/src/actions.rs
  - crates/thegn-host/src/run.rs
  - docs/help/ai-usage.md
overlaps: []
after: []
---

# Chunk 1 — notify on existing model-proxy budget caps

## Goal

Project an already-enforced `[model_proxy.budget]` cap breach into the existing
`UsageLimit` notification/attention path. This is the only implementation chunk
from THE-69's Aura audit.

Do not add `[usage].budget`, a migration, a new capability, a scheduler, a UI
panel, a provider integration, or a desktop surface. The cap authority remains
`[model_proxy.budget]`, whose schema and scope contract are in
`crates/thegn-core/src/config_model_proxy.rs:167-209`; the persisted spend rows
are in `crates/thegn-core/src/store/model_proxy.rs:49-63`.

## Exact files to touch

- `crates/thegn-core/src/budget_alert.rs` — **new pure module** containing the
  shared cap/window classification and table-driven unit tests. Accept plain
  `BudgetConfig` plus `ModelProxyBudgetStateRow` values; perform no DB, clock,
  filesystem, tokio, or UI work. Return structured breach facts, including
  scope, window anchor, spend, and which token/cost dimension crossed its cap.
  Treat a lapsed rolling window as empty using the same half-open semantics as
  the proxy. Ignore disabled configured caps; do not turn the manual kill switch
  into a cap notification in this chunk.
- `crates/thegn-core/src/lib.rs` — register the new module beside the other pure
  policy modules.
- `crates/thegn-proxy/src/budget.rs` — reuse the core cap-comparison helper in
  `check_budget`; retain the proxy's existing scope-chain, kill-switch,
  warn/refuse/downgrade, and fail-open behavior. This prevents the notification
  classifier and enforcement edge from drifting on `>=` and optional cap rules.
- `crates/thegn-host/src/usage_budget.rs` — **new host-edge adapter** that turns
  core breach facts into bounded, stable notification facts: existing kind
  `usage_limit`, a source key containing scope + window anchor + dimension, a
  concise message, and `worktree:` extraction when applicable. Keep all text
  formatting here, not in core. Unit-test source-key stability, token vs cost
  wording, worktree routing, and empty/unknown scope degradation.
- `crates/thegn-host/src/main.rs` — register the new host module.
- `crates/thegn-host/src/actions.rs` — in the existing usage worker, off the event
  loop, read `model_proxy_budget_states()` only when the model proxy and budget
  are enabled; classify them with the core module; emit each fact through
  `notify::record_global_once`. If the live route is not installed yet, use the
  documented durable-only `put_notification_once` fallback. Keep the existing
  usage payload and render path unchanged. A DB/read/classification failure must
  log at debug/warn and leave usage gathering available, never block a frame.
- `crates/thegn-host/src/run.rs` — pass the existing cloned
  `current_config.model_proxy.budget` through the three thin `spawn_usage` call
  sites (`UsagePoll`, manual refresh, and `OpenUsage`). No DB read, notification
  decision, or helper implementation may be added to `run.rs`.
- `docs/help/ai-usage.md` — clarify that configured
  `[model_proxy.budget]` token/cost caps produce one deduplicated `UsageLimit`
  notification per scope/window/dimension, while provider quota windows remain
  separate. Do not document a new key.

## Approach and invariants

The proxy currently checks the identity rollup chain and cap thresholds in
`crates/thegn-proxy/src/budget.rs:147-188`; the host already reads usage and proxy
spend off-loop in `crates/thegn-host/src/actions.rs:373-444`. Reuse those seams.
The new host projection must use the existing notification route, whose
emit-once persistence and transient-channel ordering are defined at
`crates/thegn-host/src/notify.rs:359-385,423-436`, so DND, priority overrides,
toast, sound, push, inbox, and unread badges remain one policy.

Use a stable source reference such as
`model-proxy-budget:<scope>:<window_start_ms>:<dimension>` and a stable message
for that source. Do not include changing spend numbers in the dedupe identity;
the first alert for a window is the signal, while the usage surface continues to
show current numbers. Route `worktree:<path>` facts to that worktree; global,
agent, workspace, and zone facts may use the empty worktree route unless the
existing attention model has a more specific safe mapping. Never expose prompt,
response, or provider credential data.

The feature must be additive and degrade at the edge: no budget rows means no
alert; unreadable DB means usage accounts still render; unavailable live notify
state still leaves a durable inbox row. Do not open the DB from the event loop,
change the usage payload, add a migration, or invoke the built binary against a
live state directory.

## Tests to run

Run only scoped checks, with any `thegn` invocation using a fresh temporary
`XDG_STATE_HOME`:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core budget_alert`
- `just quick thegn-proxy`
- `cargo nextest run -p thegn-proxy budget`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host usage_budget`
- `cargo nextest run -p thegn-host notify`

Do not run `just test`, `just ci`, a full-workspace compile, or e2e.

## Overlap and dependency

This is the only THE-69 coder chunk, so there is no cross-chunk parallelism. It
intentionally touches the shared budget/notification seams and must run as one
serial chunk: the proxy reuse, off-loop producer, and host routing changes are
behaviorally dependent. No other chunk may overlap these files without being
serialized after this one.

## Done criteria

- Core unit tests cover disabled budgets, token-only caps, cost-only caps, both
  dimensions, equality at the cap, lapsed rolling windows, unrelated scopes,
  and deterministic source facts.
- Proxy enforcement and host notification classification share the same cap
  comparison semantics; existing kill-switch and warn/refuse/downgrade tests
  remain green.
- A reached configured cap creates at most one `usage_limit` inbox/attention
  event per scope/window/dimension, honors existing notification routing, and
  does not repaint or block the event loop.
- Missing DB state, unavailable live routing, disabled budget, and unreadable
  usage data degrade as specified; no prompt/response content is persisted.
- No new config key, capability, migration, control-schema entry, completion
  entry, or ratchet exception is introduced. The existing help page documents the
  behavior.
- The coder commits exactly with subject:
  `feat(the-69): notify on model-proxy budget caps`
