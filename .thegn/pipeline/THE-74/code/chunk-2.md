# THE-74 — chunk 2: the pipeline board becomes its own surface

Make the board a standalone overlay that reads **left to right** as a pipeline,
and delete the monitor tab it used to live in.

## Dependencies / overlap

- **Depends on chunk 1** — calls `AgentDispatchStatus::glyph_set` and reads
  `GlyphSet::arrow_right`. Start after chunk 1 is committed.
- **File-disjoint from chunk 3**; the two may run in parallel once chunk 1 is in.
- `crates/thegn-host/src/monitor_pipeline.rs` belongs to **this chunk only**.
- `docs/help/pipeline-board.md`, `docs/help/system-monitor.md` and
  `crates/thegn-host/src/help/pages.rs` belong to **this chunk only** (chunk 3
  owns `docs/help/sidebar.md`).

## Files touched (exact)

New:

- `crates/thegn-host/src/pipeline_board/mod.rs` — the overlay
- `crates/thegn-host/src/pipeline_board/layout.rs` — **pure** board layout
- `crates/thegn-host/src/pipeline_board/view.rs` — `Line` building
- `crates/thegn-host/src/pipeline_board/action.rs` — jump + off-loop sampler
- `docs/help/pipeline-board.md`

Modified:

- `crates/thegn-host/src/main.rs` (module declaration)
- `crates/thegn-host/src/monitor_pipeline.rs`
- `crates/thegn-host/src/monitor.rs` (**deletions only** — see the split below)
- `crates/thegn-host/src/monitor/build.rs` (deletions only)
- `crates/thegn-host/src/monitor_action.rs` (deletions only)
- `crates/thegn-host/src/monitor_tests.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/handlers/overlay.rs`
- `crates/thegn-host/src/keymap_specs.rs`
- `crates/thegn-host/src/help/pages.rs`
- `docs/help/system-monitor.md`

## Approach

### 2a. Keep the fold, extend it — `monitor_pipeline.rs`

`ordered_rows` (`:110-219`), its purity and **all** of its tests stay. Its
row-identity caching contract stays (the overlay caches the row list at rebuild
and resolves the cursor by row `id`, never by index — `monitor.rs:411-413` is
the precedent).

Add to `PipelineRow` only what the new view needs and cannot re-derive:
nothing yet is required beyond what is there. If you find you need the stage's
configured facts (agent, concurrency, timeout, next), take them from
`&[PipelineStage]` in the layout function — **do not** denormalize config onto
row structs.

### 2b. `pipeline_board/layout.rs` — pure, and the heart of the chunk

```rust
pub(crate) fn board(
    rows: &[PipelineRow],
    stages: &[thegn_core::config_pipeline::PipelineStage],
    width: usize,
    now_ms: i64,
) -> Board;
```

`Board { mode: Mode, columns: Vec<Column>, … }` where
`Mode = Columns | Stacked`.

Rules:

1. **Column set** = every configured stage in declaration order (**including
   ones with no rows** — a configured-but-empty stage must be visible), then
   stages present on the roster but absent from config by name, then
   `monitor_pipeline::UNSTAGED` last. This is the same precedence
   `ordered_rows` implements at `monitor_pipeline.rs:129-145`; derive it from
   one shared helper so the two can never disagree.
2. **Mode** = `Columns` when `width >= MIN_COL_W * columns.len()`
   (`MIN_COL_W = 22`), else `Stacked`. Test both sides of the boundary and the
   zero-width case.
3. **Stage header facts** per column: `name`, `agent`, `live/concurrency`,
   `→ next`. All from config; `concurrency` and `timeout_secs` are advisory
   (`config_pipeline.rs:56-69`) — display them, never act on them.
4. **Stall cue**, pure and separately tested:
   `stalled(row, timeout_secs, now_ms) == row.status.is_active() && age_ms >
timeout_secs * 1000`. Guard the multiply against overflow. `timeout_secs == 0`
   means no budget → never stalled.
5. **Edges.** A row whose `parent_id` resolves to a row in the _previous_
   column carries an inbound edge mark; that parent carries an outbound tick.
   A row whose parent is in the _same_ column keeps `PipelineRow::depth` and
   draws tree connectors. Rows with no edge get a leading space so column
   alignment is exact.
6. **Hide-finished** toggle: a bool input; when set, terminal rows
   (`AgentDispatchStatus::is_terminal`) are dropped **after** grouping, so a
   stage's header count still reports the truth.

`layout.rs` must not import `termwiz`, `Surface`, the model, or a clock. It
takes `now_ms`. Tests live in the file.

### 2c. `pipeline_board/view.rs` — glyphs and tones

Turns a `Board` into `Vec<BoardLine>`, where
`BoardLine { line: crate::seg::Line, row_id: Option<i64> }` — `row_id` is what
makes the board hit-testable and keeps the cursor anchored to a row identity
rather than an index.

- Every glyph comes from `crate::caps::active_glyphs()`. The connectors are
  `box_h`, `box_v`, `tree_tee`, `tree_corner`, `arrow_right` (chunk 1).
  **No U+2500–U+259F literal may appear**: `test/glyph-literal-ratchet.txt` is
  shrink-only and enforced by
  `platform_ratchet_tests.rs::glyph_literals_go_through_active_glyphs`. Do not
  add an entry to that file.
- Status marks come from `AgentDispatchStatus::glyph_set(gl)` (chunk 1).
- Every tone is a `Tok::Slot(..)` / `Tok::Hue(..)`; no color literal
  (`test/color-literal-ratchet.txt`).
- The selected row is painted with `S::Accent` plus a cursor bar
  (`GlyphSet::half_block_r`) so a row visibly **looks** selectable/clickable.
- Truncation goes through `crate::seg::take_cols` / `crate::seg::cells`
  (`monitor/build.rs:1080-1087` has the `trunc` helper to copy), never byte
  slicing.

Add an ASCII-fallback test: build one board with the Unicode ladder and one
with the ASCII ladder (`caps` exposes a thread-local override — see
`caps.rs:159-174`) and assert the ASCII render is 7-bit and the same line count.

### 2d. `pipeline_board/mod.rs` — the overlay

Model it on `MonitorOverlay`, minus the tab machinery:

- `open(roster, config, screen) -> PipelineBoard`, `refresh(...)`,
  `handle_key(&KeyCode, Modifiers) -> BoardOutcome`,
  `handle_click(x, y) -> BoardOutcome`, `wheel(delta)`,
  `box_rect(screen) -> Option<Rect>`, `render(&self, &mut Surface, Rect)`,
  `take_action() -> Option<BoardAction>`, `wants_dispatches() -> bool`.
- `spec()` returns a `LayerSpec` (`monitor.rs:1268-1281` is the template):
  title `"pipeline"`, badge `" esc "`, `Anchor::Center`, `dim`, `shadow`.
  Dimensions clamped exactly the way `layer::box_dims` clamps, as
  `monitor.rs:497-511` documents — get this wrong and the tail of a long board
  is unreachable.
- Chrome: **row 0** the stage header rail, **last row** the footer legend,
  the rest the scrolled board body. Reserve them in a `CHROME_ROWS` const the
  scroll clamp also reads (`monitor.rs:104-107` precedent).
- **Keys, all of them in the footer legend** (this is the "no footer legend"
  gap — the legend must list every key the board binds, nothing more):
  - `↑`/`↓`/`k`/`j` — row
  - `←`/`→`/`h`/`l` — stage column
  - `↵` — open the row's worktree
  - `Space` — freeze / unfreeze live refresh
  - `x` — hide / show finished rows
  - `Esc` / `q` — close
  - anything else ⇒ `Passthrough`, so the opening chord still toggles it shut
    and `Ctrl-g` still locks keys (`run.rs:13706-13712` documents why).
- Freeze semantics copy the monitor's: it freezes the **view**, nothing
  underneath (`monitor.rs:20-24`).
- Neither toggle is persisted. No DB write, no new preference. Deliberate — say
  so in a comment.

### 2e. `pipeline_board/action.rs` — moved, plus the D7 fix

Move `spawn_dispatch_sample` (`monitor_action.rs:234-257`) and
`pipeline_target` (`:272-286`) here verbatim, with their tests
(`monitor_action.rs:288-334`), then **add a second resolution tier** to
`pipeline_target`:

1. as today: a `RowKind::Worktree` sidebar row with a matching
   `worktree_path` and a `tab_target`;
2. **new** — no such row: look the path up in `model.sidebar_db_worktrees`
   (`chrome.rs:458`, `sidebar::DbWorktree`) and return
   `RowTarget::Workspace { repo_path: w.repo_path, group: Some(w.tab_name) }`.
   That is the exact target the dormant-workspace path already synthesizes
   (`sidebar.rs:1229-1252`) and it flows through the same
   `handlers::sidebar_activate::activate_row_target` door (`run.rs:13652`), so
   a worktree that is registered but not open as a tab now **opens** instead of
   reporting `no open worktree for …`.
3. both miss ⇒ `None`, and the board still says so.

Tests: tier-1 hit, tier-2 hit (row absent, DB row present), tier-1 wins when
both exist, and both-miss.

Keep `PipelineJump::session` carried and unused — pane-level focus is phase 2
(`monitor.rs:92-96`).

### 2f. `run.rs` wiring

- New loop local `let mut board: Option<crate::pipeline_board::PipelineBoard> =
None;` beside `monitor` (`run.rs:6188`).
- Rewrite the `open_pipeline_board!` macro (`run.rs:6787-6825`) to toggle the
  board slot. Keep the three doors and keep the `NO_BOARD` status message for
  an empty roster with no configured stages
  (`DispatchRoster::is_present`, `monitor_pipeline.rs:80`).
- Sampler gate (`run.rs:9571-9588`): swap
  `monitor.as_ref().is_some_and(|m| m.wants_dispatches())` for the board slot.
  **Change nothing else in that block** — the cadence, the dirty flag, the seed
  pass and their comments are the 0 %-idle contract.
- Key routing: add a board arm beside the monitor arm at `run.rs:13588`, with
  the same `Passthrough` behaviour.
- The jump action: reuse the existing `MonitorAction::Pipeline` handler body at
  `run.rs:13644-13685` — close the overlay first, then activate the target,
  then report a miss into the board's notice.
- Render: paint the board next to the monitor at `run.rs:12019-12021`.

### 2g. Delete `MonitorTab::Pipeline` — as its **own final commit**

Remove: the enum variant and its arms (`monitor.rs:137-139,153,171,188,214,
241-257`), `pipeline_rows` (`:411-413,477,608-633`), `wants_dispatches`
(`:555`), `pipeline_key` (`:1240`), the `Enter` arm (`:1020`), the row-count
and nav arms (`:1033,1050,1422`), `MonitorAction::Pipeline` + `PipelineJump`
(`:86-99`), `build::pipeline` + `dispatch_tone` + the `TabInput.pipeline_rows`
field (`monitor/build.rs:59-62,99,978-1066`), and the pipeline bits of
`monitor_action.rs` / `monitor_tests.rs`.

**This must be a separate, deletion-only commit.** The THE-75 lane
(`tg/the-75-monitor-fixes`) edits `monitor.rs` for the other tabs; keeping the
deletion isolated means a conflicted merge is re-applied by redoing one small
removal instead of untangling a feature commit.

### 2h. Help + palette

- New `docs/help/pipeline-board.md` with frontmatter
  `actions: [open-pipeline-board]`. It **must mention the `Alt b` chord
  literally** — the prose ratchet (`test/help-prose-ratchet.txt`) requires a
  claimed action to be findable by chord, id, or a distinctive label word.
  Document every key in 2d and the two runtime toggles.
- Register it in `crates/thegn-host/src/help/pages.rs` `SOURCES`
  (`:11-43`) — the page is embedded with `include_str!`; forgetting this is
  how the ratchet fails.
- `docs/help/system-monitor.md:6` — drop `open-pipeline-board` from `actions:`,
  and remove the "Pipeline" tab prose.
- `keymap_specs.rs:1236-1239` — move the `"pipeline board"` /
  `"agents dispatch roster stages"` keywords off `open-monitor`; they belong to
  `open-pipeline-board` (`:1243-1257`), which already has most of them.
- Do **not** add a new action id, and do **not** edit any `test/*-ratchet.txt`.
  If a ratchet fails, the fix is the code or the page, not the allowlist.

## Tests to run (scoped)

```sh
just quick thegn-host
cargo nextest run -p thegn-host pipeline_board
cargo nextest run -p thegn-host monitor
cargo nextest run -p thegn-host render_plan
cargo nextest run -p thegn-host help
cargo nextest run -p thegn-host ratchet
```

Do **not** run `just test`, `just ci`, `just coverage`, `just e2e`, or any
full-workspace compile.

## Done criteria

- `Alt b` / the palette entry / the sidebar Pipeline row all open the **new**
  overlay; `MonitorTab::Pipeline` no longer exists.
- The board reads left-to-right: stage columns in config order, a `next`-driven
  header rail, visible parent→child edges, and every configured stage present
  even with zero rows.
- The footer legend lists every bound key; the selected row is visibly
  selected; a click inside the box selects/activates a row.
- `↵` on a row whose worktree is registered but not open **opens** it.
- `layout.rs` is pure (no termwiz, no model, no clock) with tests covering:
  column ordering, the columns↔stacked boundary, empty-stage columns, the
  stall predicate, edge classification, and hide-finished.
- `view.rs` has an ASCII-ladder test; no new entry in the glyph or color
  literal ratchets.
- The 0 %-idle wiring is unchanged apart from the gate expression.
- Two commits, in this order and with these subjects verbatim:

```
feat(pipeline-board): standalone left-to-right board surface (THE-74)
```

```
refactor(monitor): drop the Pipeline tab now that the board is its own surface (THE-74)
```
