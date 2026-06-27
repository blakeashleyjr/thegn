# Tasks

## 1. Agent pipeline (ephemeral worktree)

- [ ] 1.1 Add a pipeline orchestrator in `superzej-core` that defines the ordered
      stages and the run state machine (pending → running → parked → done/failed),
      pure and unit-testable. **Unit tests** for stage ordering, parking, and
      terminal transitions (95% line gate).
- [ ] 1.2 Reserve an ephemeral-worktree naming scheme and creation/teardown via
      `GitBackend`; ensure it is **not** registered as a sidebar tab. GC on
      completion through `worktree::clean_target`. **Unit tests** for naming and the
      GC-reclaims-orphan path.
- [ ] 1.3 Run the pipeline off-loop on `spawn_blocking`; stage transitions and
      findings send on a tokio mpsc channel and pulse the `TerminalWaker`. Assert no
      polling timeout is introduced (no new tick).
- [ ] 1.4 Degrade gracefully: with no agent/proxy, run only `test`/`lint`/`format`
  - pre-commit hooks and skip AI stages. **Unit test** the stage-selection logic
    under "AI absent."

## 2. Review-gate (finding model)

- [ ] 2.1 Add the finding type in `superzej-core`: `severity` × `action` +
      per-step `auto_fix_limit` + resolution (`approve`/`fix`/`skip`). **Unit tests**
      for the auto-fix-vs-park decision across the severity/action/limit matrix
      (review limit `0` ⇒ park).
- [ ] 2.2 Persist runs + findings in a cache table; bump SQLite `user_version`.
      **Unit tests** for round-trip persistence and gate-survives-restart rehydrate
      (isolated `XDG_STATE_HOME`).
- [ ] 2.3 Surface findings in the existing diff/review pane + `Section::Problems`;
      a gate state change triggers a `Full` frame only on transition (host-side; keep
      render-plan invariants green).
- [ ] 2.4 Expose an ACP-shaped structured resolve contract so the embedded agent
      can approve/fix/skip non-interactively (aligns with R 232).

## 3. Change-intent

- [ ] 3.1 Read intent from the agent-session↔worktree binding (no
      transcript-scraping); absent when no bound session. **Unit tests** for
      present/absent intent.
- [ ] 3.2 Feed intent into the review-gate (findings judged against intent) and
      into the PR-body generation via the existing `superzej pr create` path. **Unit
      test** PR-body section assembly from intent.

## 4. Validate

- [ ] 4.1 Run `just ci` (fmt-check + lint + build + test + coverage + smoke +
      nix-build + `openspec-validate`).
