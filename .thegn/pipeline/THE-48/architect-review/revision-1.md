# THE-48 revision 1 — restore the actionable CI autofix handoff

## Gap

The implementation has a guarded background `auto` path in
`crates/thegn-host/src/ci_autofix.rs`, and `suggest` writes a deduplicated
`pr_queue_needs_human` notification, but it never provides the promised human
action. There is no `CiAutofix`/equivalent `DetailAction`, no `f` action in the
Work CI section or CI drill, and no dispatch path from a notification/detail
action to the existing `PrCiFailure` handoff. Consequently, a user in
`mode = "suggest"` can be told that evidence is ready but cannot authorize the
handoff from the UI; the only way to act is to change policy to `auto` and wait
for another refresh.

This misses the architect design's explicit contract: `suggest` records one
deduplicated notification/action for a human, and the Work CI detail/row action
is the explicit fix action. The current help text also claims that the policy
“can suggest” a handoff without documenting a usable key or action.

## Required correction

- Add a dedicated CI autofix action (or an equivalent typed action) that carries
  enough cache-shaped identity to select the failed `(worktree, run, job, head)`
  candidate. It must be offered only for a failed run/job with available
  redacted evidence and an unclaimed candidate; an unavailable PR, stale head,
  exhausted budget, disabled policy, or missing agent must remain a visible,
  fail-closed explanation rather than a dispatch.
- Expose that action from the Work CI section and/or its in-place detail, and
  make the existing suggest notification lead to the same human-authorized
  action. Use an unclaimed/documented key (the design's `f` is suitable), wire
  it through `DetailAction`, the CI action context, run-loop dispatch, and the
  section/detail help text and relevant help/key ratchets.
- Run all provider, DB, and agent work off the compositor thread. Reuse
  `ci_autofix::consider`, the existing `PrCiFailure` prompt/`agent_run::run`
  seam, the current PR-queue agent/sandbox/timeout/attempt policy, and the
  atomic dedupe claim immediately before spawning. Do not add a new task kind,
  provider, notification enum, external write capability, or raw-log path.
- Add focused tests proving: `suggest` exposes an actionable path; firing it
  dispatches at most once for a candidate; `off` never dispatches; stale-head,
  missing-context, and exhausted-budget cases remain non-dispatching; and no
  provider or blocking DB call occurs on the render/input path.

## Scope

The implementation belongs to the existing chunk-3 host surface:

- `crates/thegn-host/src/ci_autofix.rs`
- `crates/thegn-host/src/actions.rs`
- `crates/thegn-host/src/detail.rs`
- `crates/thegn-host/src/detail/ci_drill.rs`
- `crates/thegn-host/src/panel/mod.rs`
- `crates/thegn-host/src/panel/sections/ci.rs`
- `crates/thegn-host/src/panel/section_keys.rs`
- `crates/thegn-host/src/run.rs`
- `docs/help/panel.md`

Keep the already-landed cache, provider, control/MCP, and catalog contracts
unchanged unless a narrowly necessary host-side integration fix requires it.
