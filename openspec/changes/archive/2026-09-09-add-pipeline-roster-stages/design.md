# Design — pipeline roster stages

## Shape

```text
  Lead agent (its own judgment; thegn schedules nothing)
    │ dispatch put --stage architect --session <s> ─────┐
    │ dispatch put --stage code --parent <id> …         │  agent_dispatches
    │ session open --agent … --adopt ──► daemon session │  ┌──────────────┐
    │ session wait --until done ◄───────────────────────┼─►│ id           │
    └ (writes Done/Failed for HEADLESS workers)         │  │ issue_id     │
                                                        │  │ worktree     │
  compositor: pane exit ──► dispatch_for_exit(wt, sid) ─┘  │ agent_name   │
    (writes Done/Failed for PANE workers)                  │ status       │
                                                           │ stage    v56 │
                                                           │ parent_id v56│
                                                           │ session_id v56
                                                           │ artifact_path
                                                           └──────────────┘
```

Nothing in thegn reads `stage` to decide anything. It is stored, grouped by
(part 3's board) and validated (part 2's config) — never advanced.

## Why four columns and not one JSON blob

A `meta TEXT` column would have taken all four fields with no migration
pressure, and it is the wrong trade twice over. `parent_id` and `session_id`
are **lookup keys** (the board indents by parent; the exit handler matches by
session), and a key inside a JSON blob is a table scan plus a parse. And a blob
invites the roster to become a document store: the handoff artifact is a file
**committed in the worktree**, so git — not the state cache — owns its content
and its history. `artifact_path` is a pointer, deliberately.

`parent_id` is not a SQL foreign key. The roster is a cache-side ledger (git and
the forge are the sources of truth); a pruned or hand-deleted parent must never
make a child row unreadable, which `ON DELETE`/`REFERENCES` would risk. The CLI
`dispatch put` validates the parent exists at write time — the honest place for
it, since the error can name the id.

## Render / event loop

**No render-damage channel and no wake path change.** Everything here is store
code, CLI verbs, and a wire struct. The one compositor edit is inside the
existing pane-exit `spawn_blocking` closure in `pty_drain.rs` — already
off-loop, already the place the roster status was written. The pane's daemon
session id is read from the pane table on the loop _before_ the pane leaves it
(a `Mutex` read of an already-announced `String`, the same shape as the existing
`program()` / `history_tail()` grabs beside it), then moved into the closure.
Part 3 owns the board's rendering and its `Incremental`/`Panes` invariant.

## Attribution: `dispatch_for_exit`

Two rules, in order:

1. **`session_id` exact match.** A dispatch launched through `sessions.open`
   records the daemon session running it; that is the row's identity. Matched on
   the session id ALONE, not scoped to the worktree: session ids are unique, and
   a pane's path can legitimately differ from the recorded one (a symlinked or
   non-canonical checkout), so scoping would only add a way to miss the right
   row.
2. **The most recent _active_ row for the worktree**, for a worker with no
   recorded session (the `D` key, a hand-launched agent pane). "Active" is
   `AgentDispatchStatus::is_active()` — the existing typed predicate — so the
   closed set has exactly one definition and no SQL string list can drift from
   it. `Unknown` is neither active nor terminal, so a corrupt row cannot steal
   the stamp either.

Skipping terminal rows is a **deliberate behaviour change**, and it is the bug
fix: previously any pane exit in a worktree that had ever hosted a dispatch
re-stamped the newest row (even a `merged` one from last week) and re-fired an
"agent finished" notification. Now `None` comes back and the exit routes through
the ordinary process-exit attention path, which is what a plain shell exiting
actually is.

**Division of labour** (documented at both sites): this handler stamps
`Done`/`Failed` only for workers that are _panes_. A headless session
(`session open` without `--adopt`) has no pane in this process; its terminal
status is written by the supervising agent after `sessions.wait`, the only
observer that sees it finish. The two paths never race for one row because a
row is one or the other, and `session_id` now says which.

## `--adopt`, and the finding about the intent's lifetime

`OpenSpec.adopt` makes the daemon write an `adopt_session` intent row
(`daemon/service.rs`, `AdoptIntent{session, worktree, focus}`) — "a daemon
session exists that no pane is showing; graft it in". The CLI hardcoded
`adopt: false`, which this change replaces with the flag.

**Observed today (verified, not assumed): nothing consumes that intent.**
`take_intents` is called for exactly two kinds — `focus_workspace` and
`launch_preset` (`hydrate.rs`) — and there is no `take_intents("adopt_session")`
anywhere in the tree; the only three occurrences of the string are the doc
comment on `AdoptIntent`, the `put_intent` call, and the doc comment on
`OpenSpec.adopt`. So the intent is currently **write-only in every state**, not
just when no compositor is attached:

- **No compositor attached** — the row sits in `intents`. This is the designed
  fallback ("a nudge, not a dependency"): the session stays headless and the
  supervisor can still `wait`/`snapshot`/`send` it.
- **Compositor attached** — identical, because the consumer does not exist yet.
  `--adopt` is therefore _inert at the UI today_: it costs one intent row and
  changes nothing on screen.
- **The rows accumulate.** `take_intents` is the only deleter and `intents` has
  no TTL or prune, so every `--adopt` leaves a row behind indefinitely (tiny —
  three short strings — but unbounded).

This is recorded rather than fixed here on purpose: the consumer is a compositor
concern (which group/tab the grafted pane lands in, focus policy, what happens
when the worktree has no tab open), which is part 3's territory alongside the
board. Wiring the flag now is still right — it is the door part 3 opens, and the
daemon half already exists — but **the user-visible "each stage agent appears as
a live pane" outcome is NOT delivered by this change.** Part 3 must add the
drain (`take_intents("adopt_session")` → attach the session into its worktree's
group, honouring `focus: false`) and, with it, the prune of any rows that
accumulated meanwhile. `--resume` is deferred with it.

## Insert as a params struct

`put_agent_dispatch` went from three strings to seven fields in one change.
Seven positional arguments, four of them `Option`, three of them `&str`, is the
canonical shape for silently swapping two arguments at a call site, so the
insert takes `NewDispatch<'_>` — borrowed (every caller already holds its
parts), `Copy`, with a `new(issue, worktree, agent)` constructor so a
non-pipeline dispatch stays one line and struct-update syntax covers the rest.

## Wire compatibility

`DispatchPutReq`'s four new fields are `Option` + `#[serde(default)]`, so a
client written against the three-string version is unchanged and its payloads
still deserialize. The regenerated `docs/api/control-v1.json` diff is exactly
four optional properties plus the type's doc comment: no `required` change, no
route change, no removal — the additive-only rule for /v1. `AgentDispatch`
(the response row) is `serde`-only, not `JsonSchema`, so it is not in the
snapshot; its new fields also carry `#[serde(default)]` so an older payload
round-trips.
