# Tasks

## 1. Workflow-graph model (TOML)

- [ ] 1.1 Add the node/edge model in `superzej-core`: node kinds
      (`agent-exec | check{test,lint,fmt} | approval-gate | pr`) and edge kinds
      (`sequence | conditional-on-severity | parallel | loop`) as `config_enum!`
      variants, plus the `[[pipeline.node]]` / `[[pipeline.edge]]` layered-TOML
      parse. **Unit tests** for TOML parse/round-trip and enum coverage.
- [ ] 1.2 Define the built-in **default graph** = the linear pipeline
      (intent → review → test → lint → document → approval → PR) used when
      `[pipeline]` is absent. **Unit test** that the default graph is exactly the
      linear chain (node-visit order asserted).

## 2. Pure graph executor

- [ ] 2.1 Add the pipeline engine as a **pure state machine** over the graph in
      `superzej-core` (I/O injected), same shape as `render_plan::plan`: inputs =
      graph + run state + event; output = next nodes / park / done / failed. No
      side effects. **Unit tests** for deterministic node ordering on the default
      graph and terminal transitions (95% line gate).
- [ ] 2.2 Implement `sequence`, `approval-gate` parking, and
      `conditional-on-severity` edge traversal (the day-one control-flow core).
      **Unit tests** for park-at-gate and severity-predicate branching (branch
      taken only on a qualifying finding).

## 3. Review-gate finding model (node-level policy)

- [ ] 3.1 Add the finding type in `superzej-core`: `severity` × `action` +
      per-**node** `auto_fix_limit` + resolution (`approve`/`fix`/`skip`). **Unit
      tests** for the auto-fix-vs-park decision across the severity/action/limit
      matrix (review node limit `0` ⇒ park).
- [ ] 3.2 Persist runs + node states + findings in a cache table; bump SQLite
      `user_version`. **Unit tests** for round-trip persistence and
      gate-survives-restart rehydrate (isolated `XDG_STATE_HOME`).
- [ ] 3.3 Surface findings in the existing diff/review pane + `Section::Problems`;
      an approval-gate state change triggers a `Full` frame only on transition
      (host-side; keep render-plan invariants green).
- [ ] 3.4 Expose an ACP-shaped structured resolve contract so the embedded agent
      can approve/fix/skip non-interactively (aligns with R 232), and allow parked
      findings to be handed to the `add-agent-steerable-review` panel.

## 4. Ephemeral worktree + off-loop run

- [ ] 4.1 Reserve an ephemeral-worktree naming scheme and creation/teardown via
      `GitBackend`; ensure it is **not** registered as a sidebar tab. GC on
      completion through `worktree::clean_target`. **Unit tests** for naming and the
      GC-reclaims-orphan path.
- [ ] 4.2 Run the graph off-loop on `spawn_blocking`; node transitions and findings
      send on a tokio mpsc channel and pulse the `TerminalWaker`. Assert no polling
      timeout is introduced (no new tick).
- [ ] 4.3 Degrade gracefully: with no agent/proxy, run only the `check` nodes
      (test/lint/fmt) + pre-commit hooks and skip every `agent-exec` node. **Unit
      test** the node-selection logic under "AI absent" (default graph collapses to
      test → lint → approval → PR).

## 5. Change-intent

- [ ] 5.1 Read intent from the agent-session↔worktree binding (no
      transcript-scraping); absent when no bound session. **Unit tests** for
      present/absent intent.
- [ ] 5.2 Feed intent into review/gate nodes (findings judged against intent) and
      into the `pr` node's PR-body generation via the existing `superzej pr create`
      path. **Unit test** PR-body section assembly from intent.

## 6. Follow-on control flow (model defined now; ship later)

- [ ] 6.1 Implement `parallel` edges: fan out to concurrent nodes (e.g. test +
      lint) joined before the next node, each a `spawn_blocking` task delivering
      over the same channel + waker (no new loop tick). **Unit tests** for
      fan-out/join ordering.
- [ ] 6.2 Implement `loop(fix→re-review ≤N)` edges bounded by the node's
      `auto_fix_limit`. **Unit tests** for loop termination at `N` and on a clean
      re-review.
- [ ] 6.3 Wire the blast-radius risk score from `add-semantic-blast-radius` as an
      input to a `conditional-on-severity` edge feeding a review/approval-gate node
      (T 266). **Unit test** the risk-threshold routing.

## 7. Validate

- [ ] 7.1 Run `just ci` (fmt-check + lint + build + test + coverage + smoke +
      nix-build + `openspec-validate`).
