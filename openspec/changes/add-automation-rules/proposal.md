# Extend the shipped typed-event automation engine

Linear: THE-21

## Problem

The shipped automation engine uses normalized typed facts and allowlisted
capability-catalog actions. The former proposal described an incompatible
parallel `run`/`notify`/`agent`/`invoke` DSL and a wider operator surface that
does not exist. This change makes the implemented architecture normative and
tracks only deliberate extensions to it.

## Normative shipped baseline

- Trusted global/profile `[automations]` configuration contains ordered
  `[[automations.rules]]` entries in `when … if … then …` form. Repository
  overlays cannot install persistent actions.
- Normalized facts currently cover notification, agent needs-you/finished/
  failed, PR checks/review, merge landed, worktree idle, and disk-low events.
- Actions are closed catalog capabilities: `sessions.open`, `merge.add`,
  `notify.push`, and `tools.run`, with capability-specific typed parameters.
- Evaluation is pure and bounded: selectors, templating, debounce,
  once-per-key, rule/action rate limits, explicit skip outcomes, and origin
  ancestry suppress recursive rule chains.
- Admission/state/audit writes are transactional. A bounded runtime queue and
  concurrency limit dispatch actions through the control seam with timeouts;
  queue overflow and action failures are audited and surfaced.
- `thegn automations list` reports rules/recent outcomes and `automations test`
  performs pure fixture evaluation without execution. Both are capability
  catalog, CLI, HTTP, gRPC, and MCP surfaces under their current scopes.
- Implementation landed through `94a74e35`, `fda66103`, and `462d6752`.

## Remaining work

- Add typed `session_state_changed` and `session_exited` producers.
- Add a typed scheduled-due producer with deterministic no-catch-up and
  skip-while-running semantics; it feeds the ordinary event envelope/runtime.
- Add audited history, runtime enable/disable override, and explicit run/dry-run
  operator capabilities across catalog, CLI, and control API.
- Add doctor state and hermetic live/dry-run/repo-rejection/loop/overflow E2E
  coverage, then reconcile generated API/help/config documentation.

## Non-goals

- Reintroducing arbitrary action-selected shell text; shell execution remains
  the scoped, configured `tools.run` capability.
- Letting event fields choose capability IDs, executable names, or shell
  fragments; repository-authored persistent rules; hiding caused events; or a
  general visual workflow builder.

## Overlap

THE-62 supplies sinks used by `notify.push`. THE-19 hooks are separate,
imperative lifecycle hooks; typed lifecycle facts may feed automation but hooks
do not become rules.
