# Tasks

## 1. Schema & seed model (Q 768, Q 226)

- [ ] 1.1 — Add the `scheduled_tasks` table and bump `user_version` 21 → 22 in
      `superzej-core/src/db.rs` (additive migration). **unit tests**
- [ ] 1.2 — Add CRUD + `next_fire_at` recompute helpers over `scheduled_tasks`
      in `superzej-core`. **unit tests**
- [ ] 1.3 — Extend the orchestration message type
      (`status`/`dispatch`/`worker_done`/`decision_gate`) seeded from
      `AgentDispatch` / `agent_dispatches`, with `@group` addressing. **unit tests**

## 2. Racing fan-out & compare (Q 767, Q 225, AR 563, T 267)

- [ ] 2.1 — Race coordinator: fan one prompt to N agents in N worktrees (group D
      worktree creation), reusing the proxy best-of-N fan-out (AR 563) for model
      traffic; membership keyed by seed `AgentDispatch` rows. **unit tests**
- [ ] 2.2 — Side-by-side compare of the N worktrees in the diff/review pane
      (T 267). **unit tests**
- [ ] 2.3 — Cherry-pick selected hunks into a dedicated merge worktree via the
      existing `gitmut.rs` mutations; never touch local main. **unit tests**

## 3. Message protocol surface (Q 768)

- [ ] 3.1 — Route typed messages over the orchestration mpsc channel and pulse
      the `TerminalWaker`; map to Skip/Panes/Full per `render_plan::plan`. **unit tests**
- [ ] 3.2 — Render `@group` fleet status + clickable task links via the existing
      panel link-activation path. **unit tests**
- [ ] 3.3 — `decision_gate` message blocks fan-in until resolved; resolution
      advances the run. **unit tests**

## 4. Scheduler (Q 226)

- [ ] 4.1 — Background scheduler thread computing next fire from
      preset/cron/RRULE + IANA timezone; waits off-thread, dispatches on the
      channel, pulses the waker (no loop tick). **unit tests**
- [ ] 4.2 — Target a repo (fresh worktree) or an existing worktree;
      `--reuse-session` continues in the same live terminal. **unit tests**
- [ ] 4.3 — Create-disabled → test-trigger (manual fire-now) → enable lifecycle
      via `superzej` CLI + palette. **unit tests**

## 5. AI-additive guards

- [ ] 5.1 — With no agent/proxy configured, scheduling/racing/message surfaces
      are inert (persist + render disabled), never error or block the shell. **unit tests**

## Validate

- [ ] Run `just ci`
