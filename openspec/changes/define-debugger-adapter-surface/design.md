# Design — debugger adapter and DAP surface

## Decisions to separate

1. **Launch adapters:** pure argv planning plus foreground exec can generalize
   the current CLI without implying a stateful debug UI.
2. **DAP transport:** owns request/response/event correlation, initialization,
   capability negotiation, cancellation, process/transport failure, and session
   teardown.
3. **Debugger UI provider:** owns which host surface renders stack/scopes/
   breakpoints, input routing, placement, cache/budget behavior, and degradation.

These may be accepted or rejected independently.

## Minimum launch-adapter design if adopted

- Trusted configuration only; worktree-local executable templates are rejected
  unless the central config-trust policy explicitly permits them.
- Argv arrays, never shell strings; pure placeholder validation/substitution.
- Explicit platform list, binary resolution source, run/attach support flags,
  and deterministic adapter selection.
- BugStalker remains the compatibility default and produces byte-equivalent argv.
- Doctor detects resolution/platform/capabilities but never launches an adapter.

## Minimum DAP/provider design if adopted

- One owner for adapter process and protocol state, entirely off the UI loop.
- Versioned provider messages with request ids, timeouts, cancellation, bounded
  queues/payloads, and crash/restart semantics.
- Default-deny plugin permissions separate process-launch authority from display
  contribution authority.
- Host-owned rendering tokens, placement, hit/action routing, and resource
  budgets; plugin bytes never become terminal control sequences.
- No claim that generic `PanelSection` support from THE-108 alone constitutes a
  debugger adapter or DAP implementation.

## Decision output

Record adopt/defer/reject for all three layers. Each adopted layer receives
implementation tasks, wire/config deltas, and acceptance tests in this change or
a linked child change before any public support claim changes.
