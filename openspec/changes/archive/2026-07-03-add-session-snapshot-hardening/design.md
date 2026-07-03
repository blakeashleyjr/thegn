# Design

## Scrollback capture (host + state-db)

On `session.rs` `persist()`, each leaf pane contributes a **bounded tail** of its
scrollback (the emulator already holds the grid + scrollback; a configurable
line/byte cap keeps the snapshot small). It is stored alongside the existing tab
structure via a new column on the tab-group table (`scrollback_snapshot`,
compressed text). On `resurrect()`, the tail is fed back into the pane's emulator
so the restored pane repaints its recent history before any new output arrives.
Capture and restore are off the render loop (persist already runs on the DB path).

## Stale-state coercion (core, pure)

A pure function in `superzej-core` decides restore-time state:
`coerce_stale(state, age_ms, grace_ms) -> State` — a "running"/"active" state
older than `grace_ms` is downgraded to a settled state; fresher states pass
through unchanged. It is **unit-tested** (fresh running stays running, stale
running downgrades, non-running states pass through, boundary at exactly
`grace_ms`). At restore, the `agent_dispatches` row's `dispatched_at_ms` (and the
persisted activity state) are run through this guard before the sidebar renders,
so a phantom running dot never survives resurrection.

## State (DB)

`user_version` bump: add `scrollback_snapshot` to the tab-group table and ensure
the dispatch timestamp needed for the age computation is available at restore
(the `agent_dispatches` table already carries `dispatched_at_ms`; add a TTL/grace
read at resurrection). Migration is additive (new column defaults null; old
snapshots restore with no scrollback, exactly today's behavior).

## Invariants

- **Event loop**: capture happens on the existing persist path; restore feeds the
  emulator before the first frame — no new timer, no blocking I/O on the loop.
- **Render**: a restored pane's repaint is a normal pane compose; the coerced dot
  is a `chrome dirty` repaint. render_plan invariants unchanged.
- **State**: `user_version` bump (additive column + restore-time TTL read).
- **Additivity**: scrollback is process-agnostic; the stale guard only _lowers_ an
  agent indicator's confidence and never needs the AI layer present.

## Alternatives considered

- **Persisting full scrollback** — rejected (unbounded snapshot size); a bounded
  tail gives the context benefit cheaply. Full history is the replay feature's job.
- **Clearing all agent state on restore** — rejected; it loses genuinely-recent
  state. The age-based guard keeps fresh state and only drops phantoms.
- **A background sweeper that expires stale state live** — unnecessary; the live
  `RESUME_GRACE_SECS` machine already handles running sessions. The guard is only
  needed at the resurrection boundary.
