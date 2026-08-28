# THE-74 — chunk 3: dynamic pipeline lane folders in the sidebar

Each active pipeline lane gets an auto-created, dynamically-named folder in the
left sidebar; under it, that lane's agent rows (stage + status); under each
agent, its worktree.

## Dependencies / overlap

- Depends on nothing. **May run in parallel with chunk 1** (that chunk is
  `thegn-core`-only; this one is `thegn-host`-only) and with chunk 2.
- **Do not touch** `crates/thegn-host/src/monitor_pipeline.rs` — chunk 2 owns
  it. Put this chunk's fold in a new `crates/thegn-host/src/sidebar_pipeline.rs`.
- **Do not touch** `docs/help/pipeline-board.md`, `docs/help/system-monitor.md`
  or `crates/thegn-host/src/help/pages.rs` — chunk 2 owns those. This chunk
  owns `docs/help/sidebar.md` only.

## Files touched (exact)

New:

- `crates/thegn-host/src/sidebar_pipeline.rs` — **pure** lane fold + tests

Modified:

- `crates/thegn-host/src/main.rs` (module declaration)
- `crates/thegn-host/src/sidebar.rs`
- `crates/thegn-host/src/sidebar_view.rs`
- `crates/thegn-host/src/attention_status.rs`
- `crates/thegn-host/src/handlers/sidebar_keys.rs`
- `crates/thegn-host/src/handlers/sidebar_mouse.rs`
- `docs/help/sidebar.md`

## Approach

### 3a. `sidebar_pipeline.rs` — the pure fold

```rust
pub(crate) struct Lane {
    pub key: String,        // stable identity
    pub label: String,      // what the folder shows
    pub agents: Vec<LaneAgent>,
}
pub(crate) struct LaneAgent {
    pub id: i64,            // roster row id
    pub stage: String,
    pub agent_name: String,
    pub status: AgentDispatchStatus,
    pub worktree_path: String,
    pub worktree: String,   // basename
    pub dispatched_at_ms: i64,
}

pub(crate) fn lanes(
    dispatches: &[AgentDispatch],
    stage_order: &[String],
) -> Vec<Lane>;
```

Rules — all pure, all tested in-file:

1. A lane exists **only while it has active rows**
   (`AgentDispatchStatus::is_active`, `issue.rs:377-382`). Terminal rows are
   dropped before grouping, so a finished lane disappears on its own with no
   reaper anywhere.
2. **Lane key** = the row's `issue_id` when non-empty and non-blank, else the
   basename of that row's `worktree_path` (`thegn_core::util::basename`). Rows
   with neither are skipped.
3. **Lane label** = `"{issue_id} · {basename}"`, or just the basename when
   there is no issue id — "the issue id + short title, derived from the
   roster's issue_id/worktree". The basename is the earliest active row's, so
   the name is stable as the lane advances. Truncate on the render side, not
   here.
4. **Lane order** = by earliest active `dispatched_at_ms`, oldest lane first
   (the order work started — the same reading `ordered_rows` uses), tie-broken
   by key so the tree never reshuffles frame to frame.
5. **Agent order within a lane** = configured stage order first (a stage not in
   `stage_order` sorts after the named ones, by name), then
   `dispatched_at_ms`, then row `id`.
6. A worktree may repeat across agents in a lane — that is correct and
   intended, each agent row carries its own worktree child.

Tests: lane appears/vanishes with active rows; label with and without an
`issue_id`; a blank/whitespace `issue_id` falls back to the basename; lane and
agent ordering; two lanes are not merged; a row with an empty worktree path and
no issue id is skipped.

### 3b. Feed it — `attention_status.rs`

The roster is already read off-loop at `attention_status.rs:194` and folded
three ways (`:199-201`, `stage_blocked` at `:202`). Add a **fourth** derivation
over the same rows:

```rust
status.pipeline_lanes = crate::sidebar_pipeline::lanes(&roster, &stage_order);
```

The stage order comes from config; thread it in the same way that function
already receives config, or take it from the caller — **do not open the DB
again and do not spawn anything.** This is the "no new wake source" invariant:
the fold runs over rows already in memory on a thread that already exists.

Add `pipeline_lanes: Vec<Lane>` to `SidebarStatus` (`sidebar.rs:402-412`,
beside `pipeline_stages` and `pipeline`).

### 3c. Three new row kinds — `sidebar.rs`

```rust
RowKind::PipelineLane      // the dynamic folder; collapsible
RowKind::PipelineAgent     // one roster row; collapsible
RowKind::PipelineWorktree  // leaf; carries the real tab_target
```

Why new kinds instead of reusing `RowKind::Folder` + `RowKind::Worktree` — two
concrete hazards, both verified:

- `SidebarRow::is_markable` (`sidebar.rs:326-329`) and the mark set
  (`handlers/sidebar_keys.rs:904,922,969-977`) key on `pin_key`. A mirrored
  `RowKind::Worktree` sharing a primary row's `pin_key` makes a bulk action
  count one worktree twice, and `sidebar_keys.rs:544`'s
  `.position(|r| r.pin_key == target_key)` becomes ambiguous. This is the
  identity-anchor / pin-key trap the sidebar audit records.
- Chunk 2's `pipeline_target` finds the _first_ `RowKind::Worktree` row with a
  matching path; a mirror of that kind would make the board's jump depend on
  emission order.

`RowKind::Folder` is also wrong for the lane: it implies a `folders` row with a
`folder_id` and a user-owned `position`, and lane rows carry
`folder_id: None` — the file/unfile/rename paths would then have a Folder row
they cannot resolve.

Required wiring for the new kinds:

- `RowKind::is_collapsible` (`sidebar.rs:44-51`) — add `PipelineLane` and
  `PipelineAgent`.
- `SidebarRow::collapse_key` (`sidebar.rs:331-338`) — both new collapsible
  kinds key on their own `pin_key`, exactly as `Folder` does.
- `is_markable` needs no change: it matches only `Workspace | Worktree`, so all
  three new kinds are unmarkable by construction. Assert that in a test.
- `pin_key`: `"pipeline/lane:{key}"` for the lane,
  `"pipeline/lane:{key}/agent:{id}"` for the agent — used **purely** as a
  collapse key, never as a pin. `RowKind::PipelineWorktree` carries an
  **empty** `pin_key`, which every pin/mark path already skips
  (`sidebar_keys.rs:303,347,424`).
- `child_count` on the lane row = its agent count (drives the folder count
  rendering).

### 3d. Emit them — `sidebar.rs`

Emit **immediately after** the existing `RowKind::PipelineSummary` door row
(`sidebar.rs:1012-1024`), i.e. still at the tail, just above the TERMINALS
banner. **Keep that placement and its comment**: it is load-bearing — the
sidebar cursor is a visible-row index, so a head placement shunts the cursor
off the row under it every time an agent starts or finishes.

Keep `RowKind::PipelineSummary` exactly as it is. It stays the door (Enter and
click already synthesize `Action::OpenPipelineBoard` —
`sidebar_keys.rs:635-637`, `sidebar_mouse.rs:278-283`) and now also reads as
the section head above the lanes.

Row shape:

```
Pipeline ▸ 4 running          PipelineSummary   (unchanged)
  THE-74 · tg-the-74-…        PipelineLane      depth 1, collapsible
    architect · claude  ✓ 2h  PipelineAgent     depth 2, collapsible
      tg-the-74-pipeline-…    PipelineWorktree  depth 3, leaf
    code · claude       ● 4m  PipelineAgent
      tg-the-74-pipeline-…    PipelineWorktree
```

Visibility follows the folder precedent (`sidebar.rs:983-1006`): children are
**always emitted**, with `visible` toggled off when an ancestor is collapsed,
so the sidebar filter can still find and reveal a row inside a collapsed lane.

`RowKind::PipelineWorktree` must carry the real `worktree_path` **and** the
real `tab_target`, resolved the same way the primary rows are — reuse the
`Group`/`DbWorktree` lookups already built in `build_rows`
(`sidebar.rs:828-842`, `gather_groups` at `:1173`). When no target resolves,
leave `tab_target: None` and render the row dim rather than omitting it.

### 3e. Render — `sidebar_view.rs`

- `draw_row` (`sidebar_view.rs:1439` region): three new arms. Lane = caret +
  label + child count, keyed off `row.collapsed`. Agent = status glyph +
  `stage · agent` + age. Worktree = the tree connector + basename.
- Rail (narrow) mode (`sidebar_view.rs:1684` region): lanes/agents/worktrees
  have no meaningful rail form — fall through to the existing faint-divider
  `_` arm, or render the lane as a count. Do not invent a glyph.
- **Every glyph from the `gl: &GlyphSet` already in scope** —
  `caret_open`/`caret_closed`, `tree_tee`/`tree_corner`, `dot_filled`,
  `attention`, `check`, `cross`. No literal: `test/glyph-literal-ratchet.txt`
  is shrink-only and enforced; do not add an entry.
- Every tone is a `Tok::Slot` / `Tok::Hue`; no color literal.
- Ages: reuse the existing relative-age formatting rather than writing a
  second one.

### 3f. Interaction — `handlers/sidebar_keys.rs`, `handlers/sidebar_mouse.rs`

- `sidebar_keys.rs:442` and `:635` — the new kinds join the existing switch.
  `↵` on a lane or agent **toggles collapse** (they are collapsible);
  `↵` on a `PipelineWorktree` **activates its `tab_target`**, hitting the same
  `SidebarOutcome::Activate` path a primary worktree row takes
  (`sidebar_keys.rs:639-641`). That is what "worktree rows keep their normal
  identity/hit-targets" means here: same target, same door.
- `sidebar_mouse.rs:278` — a click behaves identically to `↵` for each kind.
- Neither the lane nor the agent row may reach a pin, mark, reorder, rename,
  close or delete action. Add a test asserting `is_markable()` is false for all
  three kinds and that a lane row is skipped by the pin path.

### 3g. Help

`docs/help/sidebar.md` — a short section describing the lane folders: they are
derived from the dispatch roster, appear while a lane has active agents, vanish
on their own, and cannot be renamed, reordered or filed (they are not real
folders). No new action id, so `test/help-ratchet.txt` is unchanged — **do not
edit any ratchet file**.

## Tests to run (scoped)

```sh
just quick thegn-host
cargo nextest run -p thegn-host sidebar
cargo nextest run -p thegn-host pipeline
cargo nextest run -p thegn-host ratchet
```

Do **not** run `just test`, `just ci`, `just coverage`, `just e2e`, or any
full-workspace compile.

## Done criteria

- A lane with active roster rows produces a named folder; the folder disappears
  when its last active row goes terminal — with no DB write anywhere in this
  chunk.
- Under the lane: one agent row per roster row (stage + agent + status + age),
  and under each agent its worktree, activating exactly like the primary row.
- `sidebar_pipeline::lanes` is pure and tested for identity, naming, ordering
  and the appear/vanish rule.
- Lanes/agents collapse and expand, and their collapse state survives a rebuild
  (it lives in `ViewState::collapsed`, keyed on `pin_key`).
- No new row kind is markable, pinnable or reorderable; a test asserts it.
- The tail placement of the pipeline section is unchanged, and the existing
  `PipelineSummary` door still opens the board.
- No new entry in any `test/*-ratchet.txt`.
- Note for the reviewer, not a task: if an e2e baseline is ever re-recorded,
  the lane rows' relative age is volatile chrome and must be pinned in
  `e2e_freeze.rs`. Do not run e2e in this lane.

## Commit

Exactly one commit, this subject verbatim:

```
feat(sidebar): dynamic pipeline lane folders with agents and worktrees (THE-74)
```
