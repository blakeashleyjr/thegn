# Design

Two cooperating subsystems sit on top of existing seams:

- **The racer** fans a single prompt to N agents in N worktrees, reusing the
  proxy best-of-N fan-out (AR 563) for the model traffic and worktree creation
  (group D) for the workspaces. Results are compared in the diff/review pane
  (T 267) and folded into a dedicated **merge worktree** via cherry-pick using
  the existing git mutations in `gitmut.rs`. The race does **not** touch local
  main — that is the separate fold-actor merge-queue.
- **The scheduler** is a single background thread that owns the next-fire clock
  for `scheduled_tasks` rows (cron / RRULE / IANA timezone). When a task is due
  it dispatches a prompt run (new worktree, or `--reuse-session` into an
  existing live terminal) by sending on the orchestration channel.

The **message bus** (`status`/`dispatch`/`worker_done`/`decision_gate`) is the
glue: every off-loop producer (racer workers, scheduler, dispatch transitions)
emits typed messages addressable by `@group`, seeded by the existing
`AgentDispatch { issue_id, worktree_path, agent_name, status }` record and the
`agent_dispatches` table (v12).

## Rendering & event loop

All orchestration work happens off the main loop. Producers — the racer worker
threads, the scheduler thread, and `AgentDispatch` status transitions — send on
a tokio mpsc channel and **pulse the `TerminalWaker`**. The loop drains the
channel on wake and re-renders only when dirty. There is **no tick and no
polling timeout** on the loop; the scheduler computes the duration to the next
fire on its own thread and waits there, never on the loop.

Mapping to the three damage channels (`render_plan::plan`):

- **Skip** — an idle wake with no orchestration message and no due task; the
  0%-idle contract holds.
- **Panes** — a racing agent streams output into its worktree's pane; only that
  pane's PTY content is dirty (`dirty_panes`), so the frame is one
  `compose_pane` + a bounded `diff_region`, never a chrome recompose.
- **Full** — a `status`/`worker_done`/`decision_gate` message that changes the
  fleet badge, dispatch chip, sidebar dot, or opens the side-by-side
  compare/merge surface marks the master `dirty` (chrome) and re-renders via
  `render_tab` + `diff_screens`.

The side-by-side compare reuses the existing diff/review pane (T 267); the
clickable task links resolve through the same panel link-activation path as
existing PR/issue links.

## Persistence

One new table; SQLite `user_version` bumps **21 → 22** (additive migration, no
backfill of existing rows required):

```
scheduled_tasks(
  id              INTEGER PRIMARY KEY,
  repo_id         INTEGER NOT NULL,         -- target repo
  worktree_path   TEXT,                     -- NULL => create a fresh worktree
  prompt          TEXT NOT NULL,
  agent_name      TEXT,                     -- NULL => default agent
  schedule_kind   TEXT NOT NULL,            -- 'preset' | 'cron' | 'rrule'
  schedule_expr   TEXT NOT NULL,            -- preset name, cron string, or RRULE
  timezone        TEXT NOT NULL,            -- IANA tz id, e.g. 'America/Chicago'
  reuse_session   INTEGER NOT NULL DEFAULT 0,
  enabled         INTEGER NOT NULL DEFAULT 0, -- created DISABLED
  last_fired_at   INTEGER,                  -- unix epoch, NULL until first fire
  next_fire_at    INTEGER,                  -- precomputed, NULL while disabled
  created_at      INTEGER NOT NULL
)
```

git remains the source of truth for worktrees; `scheduled_tasks` is purely the
schedule definition. Racing is **transient**: race membership and per-worker
results live in memory keyed by the seed `AgentDispatch` rows in
`agent_dispatches` (v12); no new race table is added. The merge worktree is an
ordinary worktree managed by the existing worktree lifecycle.

## Invariants

- **0% idle is preserved.** No polling timeout, no tick: the scheduler waits
  off-thread until the next fire and pulses the `TerminalWaker`; an idle wake
  resolves to `Skip`. Racing pane output resolves to `Panes` (bounded diff), not
  a chrome recompose.
- **AI is strictly additive; never a hard dependency.** The orchestration layer
  is optional. The AI-free shell MUST build and run with it absent or disabled.
  When no agent or proxy is configured, scheduled tasks, racing, and the message
  bus are inert (rows persist, surfaces render disabled) and MUST NOT error or
  block the shell.
- **No new merge path.** Cherry-pick into a merge worktree only; local main is
  untouched (the fold-actor merge-queue owns folding into main).
- **Seed record reuse.** Fleet membership and message addressing derive from
  `AgentDispatch` / `agent_dispatches` (v12); orchestration adds no parallel
  state machine for dispatch status.
