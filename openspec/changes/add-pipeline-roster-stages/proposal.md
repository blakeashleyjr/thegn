# Add pipeline stage columns to the dispatch roster

Part 1 of 3 of the agent-pipeline plan (Lead → Architect → Coders → Reviewer →
Merger), the DB + verbs + watchability layer that parts 2
(`add-pipeline-config-and-skill`) and 3 (`add-pipeline-board`) build on.

## Why

The durable roster (`agent_dispatches`, v12) records one flat fact: this agent
is working this issue in this worktree. A multi-stage pipeline needs three more
that have nowhere to live today:

- **Which stage a row is** — architect, code, review, merge. Without it a board
  cannot group rows, and a supervisor resuming after a crash cannot tell an
  architect's row from the coder rows it fanned out.
- **Which row a chunk came from** — the Architect emits n chunks, each dispatched
  to its own coder. The parent→child edge exists only in the Lead's context
  window, which is exactly the thing a crash destroys.
- **Which session runs a row, and where its handoff artifact is.** Both are the
  identity a later observer needs: the session id to attribute a pane exit, the
  artifact path to read what the previous stage decided.

And one live bug blocks the whole shape: `dispatch_for_worktree` /
`dispatch_info_for_worktree` resolve a worktree to its **most recent row**, and
`pty_drain.rs` stamps `Done`/`Failed` onto whatever that returns. The moment two
stages share one worktree — the normal case for a pipeline — the wrong row is
stamped. Worse, terminal rows are eligible: a plain shell opened later in a
worktree that once hosted an agent re-stamps a finished row and re-fires an
"agent finished" notification for work that ended days ago.

Finally, a CLI-opened stage agent is invisible: `cmd/session.rs` hardcodes
`adopt: false`, so an agent dispatched from the CLI never becomes a pane you can
watch, even with a compositor attached and `OpenSpec.adopt` already implemented
on the daemon side.

## Doctrine

**The roster gains columns, never transitions.** `stage` is structure (the org
chart), not judgment. thegn stores it, groups by it, and renders it; **no thegn
code path advances a stage, enforces a concurrency limit, or fires a stage
timeout** — the Lead agent reads the roster and decides, exactly as
`add-agent-orchestration-surface` (THE-57) decided when it **rejected a native
drain driver**: "every driver feature hard-codes judgement the prompt should
own", and "the roster is the supervisor's ledger, not a state machine". This
change is the complement of that decision, not a retreat from it: four nullable
columns and one attribution fix, with the scheduling left where it was put.

That is also why there is no `dispatches.update` verb. `put` carries every
column, so a mutable stage field never exists for thegn to be tempted to
advance.

## What Changes

1. **Schema v56 (additive)** — `agent_dispatches` gains `stage TEXT`,
   `parent_id INTEGER`, `session_id TEXT`, `artifact_path TEXT` via four
   idempotent `ALTER`s in `db_migrate::additive_schema`; `SCHEMA_VERSION`
   55 → 56, stamped last. Every pre-v56 row reads back `None`, which is exactly
   the pre-change behaviour. `artifact_path` is a **pointer** to a file committed
   in the worktree — git stays the source of truth, so the roster never becomes a
   document store (no meta-JSON blob column).
2. **`AgentDispatch` + the store move together** — the four fields land on the
   struct, on both explicit-column reads (now sharing one column list + row
   mapper so they cannot drift), and on the insert, whose signature becomes a
   `NewDispatch` params struct rather than seven positional arguments.
3. **`dispatch_for_exit(worktree, session_id)`** — a new store function that
   resolves a finished worker's row by **session id first**, else the most recent
   **active** row for the worktree, skipping terminal rows. `pty_drain.rs`
   switches to it. Headless sessions keep their existing division of labour:
   their statuses are written by the supervising agent after `sessions.wait`,
   because a session with no pane has no pane exit to observe.
4. **Additive wire + CLI** — `DispatchPutReq` gains the four fields as
   `#[serde(default)]` options (the control-schema snapshot is regenerated;
   the diff is four optional properties, no `required` change, no route change);
   new `thegn dispatch put <issue> <worktree> <agent> [--stage --parent
--session --artifact] [--json]`, DB-direct like its `dispatch` siblings.
5. **`thegn session open --adopt`** — the flag that stops hardcoding
   `OpenSpec.adopt = false`. Default stays `false`; the pipeline skill (part 2)
   always passes it. **Caveat found while wiring it, recorded in design.md:**
   nothing in the tree consumes the `adopt_session` intent the daemon writes
   (`take_intents` is called for `focus_workspace` and `launch_preset` only), so
   the flag is inert at the UI until part 3 adds the drain. The flag and its help
   text say so rather than promising a pane.

## Impact

- **Roadmap**: Q 212 (task→worktree→agent→review→merge pipeline — the stage/
  parent edges it needs), Q 213 (agent registry + normalized states — the
  attribution fix), Q 223 (task history/audit — a roster that records the shape
  of the work), Q 224 (batch/parallel launch — the parent→chunk fan-out edge).
- **Specs**: `state-db` (ADDED: pipeline columns, exit attribution),
  `cli` (ADDED: `dispatch put`, `session open --adopt`), `control-plane`
  (ADDED: `dispatches.put` carries the pipeline columns).
- **No new capability rows.** `capability.rs` and the control verb tables are
  **byte-unchanged**: `dispatches.put` already exists and simply carries more
  fields. No new CLI noun either (`dispatch` is already the Forge group's), so
  `cli_help::GROUPS` is unchanged, and no TUI action/keybind/panel section is
  added, so the help ratchets are unchanged.
- **In-flight changes**: depends on `add-agent-orchestration-surface` (THE-57 —
  the roster, `AgentDispatchStatus`, `dispatches.*`, `session open`), which is
  landed but not yet archived; part 2 (`add-pipeline-config-and-skill`) and part
  3 (`add-pipeline-board`) consume these columns and are parallel with each other
  after this lands. Part 3 supersedes the dormant `add-fleet-view`.
- **AI-free shell**: strictly additive. With no agent configured the columns are
  simply never written; every existing caller of the roster keeps its behaviour,
  and the attribution fix makes the no-agent case _quieter_ (a shell in an
  ex-agent worktree stops minting agent notifications).
- **DB**: one additive schema bump (v55 → v56), no data reset, no backfill.
