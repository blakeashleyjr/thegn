# Tasks — typed-event automations

## 1. Shipped normative engine

- [x] 1.1 Implement normalized typed events, predicates, pure evaluation,
      explicit skip outcomes, bounded templating, debounce, once keys, and
      rule/action rate limits with core tests.
- [x] 1.2 Implement trusted global/profile `when/if/then` configuration,
      repo-layer rejection, and the closed `sessions.open|merge.add|notify.push|
tools.run` action/parameter schema.
- [x] 1.3 Implement transactional ledger/admission/audit storage and bounded
      retention with concurrency/state tests.
- [x] 1.4 Implement bounded runtime queue/concurrency/timeouts, overflow audit,
      catalog action dispatch, origin propagation/suppression, and failure
      notifications.
- [x] 1.5 Wire shipped notification, agent, PR/review, merge, idle, and disk
      producers through the normalized event seam.
- [x] 1.6 Catalog and project `automations.list` and pure `automations.test`
      through CLI, HTTP, gRPC, and MCP with coverage tests.

## 2. Remaining typed producers

- [ ] 2.1 Add typed `session_state_changed` and `session_exited` kinds and
      authoritative edge producers using the existing envelope/origin/audit
      model.
- [ ] 2.2 Add a scheduled-due producer with deterministic no-catch-up and
      skip-while-running auditing; dispatch only through the existing runtime.

## 3. Remaining operator surfaces

- [ ] 3.1 Add scoped catalog, CLI, and control capabilities for audit history,
      runtime enable/disable overrides, and explicit run/dry-run.
- [ ] 3.2 Add doctor output for active/inert rules, daemon dependency, trusted
      source, and channel/action restrictions.

## 4. Remaining verification

- [ ] 4.1 Add hermetic end-to-end coverage for one live action, pure dry-run,
      repo rejection, loop suppression, and queue-overflow audit.
- [ ] 4.2 Reconcile generated API docs, help, config examples, and final task
      state with the implemented surfaces.

## 5. Normative reconciliation

- [x] 5.1 Replace the superseded arbitrary-action/scheduler proposal with this
      behavior-first typed-event/catalog-action contract.
- [ ] 5.2 Validate the completed change strictly before archive.
