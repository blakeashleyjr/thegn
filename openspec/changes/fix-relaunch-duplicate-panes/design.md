## Context

There is exactly one door a daemon session becomes a pane:
`Panes::spawn_daemon_backed` with `attach = Some(session)`. Two independent
callers route through it on relaunch:

- Warm-reattach: `materialize_with_specs` sees the leaf's persisted
  `pane_sessions` record and reattaches that session onto its own leaf.
- Attach-on-open (THE-85): the off-loop workers probe the daemon's live
  sessions for the worktree and ship them on the `SpecBatch`; the drain plans
  them onto the tab's missing leaves, and the surplus beyond those leaves
  grafts in as splits via `adopt::graft`.

The workers probe with an empty `shown` (they cannot see this process's
panes), so the drain re-dedups against `panes.table` before planning. That
dedup only knows LIVE panes — it cannot see that a recorded session is
about to reattach onto a leaf of this same restore. THE-2683 is that window:
the probe lists the recorded session (alive, same worktree), the drain finds
no pane showing it, materialize reattaches it onto its leaf, and the surplus
graft splits the same session in a second time.

## Goals / Non-Goals

**Goals**

- A relaunch restores each recorded daemon session exactly once: onto its
  own leaf, with the tab's persisted tree shape.
- A live headless session (no record) still lands on the tab's missing leaf.
- The prewarm-skip adopt path carries the same guarantee.

**Non-Goals**

- Changing the probe contract or the workers: the probe stays connect-only
  and keeps passing an empty `shown`.
- Touching the warm-reattach branch or the one-door invariant: the fix plans
  the pool, it does not add an attachment path.
- Multi-worker duplicate attach races (two tabs' workers probing the same
  headless session concurrently) — the daemon claims per session on attach,
  and that pre-existing behavior is unchanged.

## Decisions

### The drain owns the dedup against records

`worktree_attach::recorded_daemon_sessions(session)` collects every daemon
session id a `pane_sessions` entry names whose key is a live leaf of its
tab's tree — the exact promise materialize will honor. `drain_specs` filters
the probe targets against that list beside the existing `shown` filter, and
`attach_prewarm_sessions` does the same. Pure (a fold over the session
model), no I/O, no DB: the drain never opens the state DB, and this fix
keeps that invariant.

### Stale records stay adoptable

A record keyed by a leaf that is no longer in the tree (a pane closed while
the session lived on) does not filter: from the compositor's view its
session is headless, and dropping it would strand a live session the user
expects to find after a `thegn attach`. Only live-leaf records claim.

### Every group and tab counts, not just the batch's

The filter spans the whole session model — a neighboring tab's recorded
session reattaches when ITS tab materializes later in the same restore, so
keeping it out of this tab's pool prevents the mirror-image duplicate one
batch later. Stale-leaf records and remote-provider records are ignored.

### No render or wake-path change

The drain already sets `need_relayout`/`dirty` on the materialize path; the
filter only changes what lands in the surplus. No new render damage channel,
no event-loop wake path, no SQLite schema or migration is touched (the DB
is a resurrection cache; git remains the source of truth).
