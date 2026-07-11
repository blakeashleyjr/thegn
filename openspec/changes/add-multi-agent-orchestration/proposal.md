# Add multi-agent orchestration

## Summary

Give thegn a way to run **several agents against one task** and to
**coordinate fleets** of them, on a different axis from the fold-actor local
merge-queue (which folds N already-finished worktree branches into local main).
Three capabilities:

1. **Multi-agent racing on one task** — fan out N agents into N worktrees on the
   _same_ prompt, surface their results side by side in the existing diff/review
   pane, then cherry-pick the best hunks into a dedicated **merge worktree**.
   This realizes best-of-N and ties the proxy's best-of-N fan-out for the racing
   model traffic.
2. **Orchestration message protocol** — a typed message bus
   (`status`/`dispatch`/`worker_done`/`decision_gate`) with `@group` addressing
   and clickable task links, seeded by the existing `AgentDispatch` record so
   fleet state is observable and routable.
3. **Scheduled prompt runs** — schedule a prompt against a repo or an existing
   worktree (presets, raw cron, RRULE, IANA timezone), with `--reuse-session` to
   continue in the same live terminal, gated by a deliberate
   create-disabled → test-trigger → enable lifecycle.

Off-loop work (the scheduler, the race fan-out fan-in) follows the event-loop
rule: a background thread sends on an mpsc channel and pulses the
`TerminalWaker`; there is **no polling timeout** added to the main loop.

## Impact

Roadmap items (tasks.md) this change gives concrete behavior to:

- **Q 767** — Multi-agent racing on one task (N worktrees, same prompt →
  side-by-side diff → cherry-pick into a merge worktree).
- **Q 768** — Orchestration message protocol
  (status/dispatch/worker_done/decision_gate, `@group`, clickable task links).
- **Q 225** — Best-of-N attempts (now realized by 767).
- **Q 226** — Scheduled/cron tasks (presets + cron + RRULE + IANA timezone;
  target a repo or worktree; `--reuse-session`; create-disabled → test → enable).

Relates to:

- **Q 211 / 212 / 215 / 224** — task creation, task→worktree→agent→review→merge
  pipeline, and dispatch surfacing.
- **T 267** — cycle / side-by-side diffs (the racing compare surface).
- **AR 563** — proxy best-of-N fan-out (reused for the racing model traffic).

New capability introduced (ADDED specs): `agent-orchestration`.

New DB state: a `scheduled_tasks` table; SQLite `user_version` bumps 21 → 22.

## Rationale

- **thegn is worktree-native and already spins per-worktree sandboxes**, so
  N racing agents map cleanly onto N worktrees and the existing diff/review pane
  (T 267) is already a side-by-side compare surface. Cherry-pick into a merge
  worktree reuses the existing git mutations in `gitmut.rs`.
- **The dispatch record already exists.** `AgentDispatch` and the
  `agent_dispatches` table (v12) are the natural seed for fleet membership and
  for the message bus, so the protocol observes real state rather than inventing
  a parallel one.
- **The proxy already fans out best-of-N** (AR 563); the racer reuses that path
  for model traffic instead of duplicating fan-out logic.
- **Scheduling is the only new persistent state**, and the
  create-disabled → test → enable lifecycle prevents a mistyped cron expression
  from silently spawning agents.

## Non-goals

- **No new merge mechanism.** Racing reuses cherry-pick into a worktree; it does
  not fold branches into main (that is the separate fold-actor merge-queue).
- **No always-on daemon process.** The scheduler is an in-process background
  thread of the single compositor, not a separate service.
- **No AI hard-dependency.** Orchestration is strictly additive: the AI-free
  shell MUST build and run with the orchestration layer absent or disabled, and
  scheduling/racing surfaces MUST be inert (not error) when no agent or proxy is
  configured.
