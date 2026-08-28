# THE-74 — Pipeline board v2: architect design

Branch `tg/the-74-pipeline-board-v2`. Written 2026-08-27 against
`982ab7cb`.

Everything below cites `file:line` from this worktree. Where a claim is about
live data it was checked against the real DB, read-only.

---

## 0. What is actually there today (evidence)

The "pipeline board" is a **tab inside the system-monitor modal**, not a
surface:

- `crates/thegn-host/src/monitor.rs:137-139` — `MonitorTab::Pipeline`, hidden
  until `has_pipeline`.
- `crates/thegn-host/src/monitor.rs:245-257` — `present()` gates the tab on
  `DispatchRoster::is_present`.
- `crates/thegn-host/src/monitor.rs:555` — `wants_dispatches()` is the sampler
  gate; `:1240` `pipeline_key()` raises the jump; `:1288` `render` paints the
  shared tab chrome.
- `crates/thegn-host/src/monitor/build.rs:1002-1066` — `pipeline()`: a
  `heading` + **one `TableSection` per stage**. Each table sizes its own
  columns, which is exactly why column alignment breaks between stages
  (`sections.rs:185` `TableSection` is per-section).
- `crates/thegn-host/src/monitor_pipeline.rs:110-219` — `ordered_rows`, the
  pure fold. It is good and stays.
- `crates/thegn-host/src/monitor_action.rs:234-286` — the off-loop sampler and
  `pipeline_target`.
- `crates/thegn-host/src/run.rs:6787-6825` — the `open_pipeline_board!` macro
  (three doors: action, sidebar `↵`, sidebar click) that _drives the monitor_.
- `crates/thegn-host/src/keymap_specs.rs:1243-1257` + `keymap.rs:1277` — the
  `open-pipeline-board` action and its `Alt b` chord already exist.
- `docs/help/system-monitor.md:6` — `actions: [open-monitor, open-pipeline-board]`.

The parent→child DAG is folded (`monitor_pipeline.rs:154-217`) but rendered as
**two spaces of indent** (`monitor/build.rs:1043`), so the graph is invisible.

Status glyphs are hard-coded in core and bypass the caps ladder:
`crates/thegn-core/src/issue.rs:384-395` — `Queued` and `Spawning` and
`Running` all return `"⚙"`, and there is no `GlyphSet` variant, so
`[theme] glyphs = ascii` renders mojibake.

The seconds/milliseconds bug is confirmed against the live DB
(`~/.local/state/thegn/thegn.db`, read-only):

```
1|1787811400      ← seconds
2|1787811400
…
MAX|42
BAD|28            ← rows with dispatched_at_ms > 0 AND < 1e11
```

The **write** side is already fixed —
`crates/thegn-core/src/db_notification.rs:299-320` inserts `util::now_ms()`
and carries the comment explaining the old `util::now()` bug — and
`put_agent_dispatch` is the only writer of the column. What never happened is
the **migration** of the 28 legacy rows, and there is no read-side guard:
`db_notification.rs:463` reads the column raw, and
`monitor_pipeline.rs:233` turns it into an age that reads ~20 671 d.

---

## 1. Decisions

### D1 — The board becomes its own overlay; the monitor tab is deleted

New module `crates/thegn-host/src/pipeline_board/`. It is a boxed layer of its
own, opened by the existing `open-pipeline-board` action / `Alt b` / the
sidebar door. `MonitorTab::Pipeline` is **removed**, not aliased: an alias
means the tab bar still shows "Pipeline", still steals `Alt b`'s toggle
semantics, and still owns the sampler gate — three ways for the two surfaces to
disagree. Deleting it is a set of removals in `monitor.rs` /
`monitor/build.rs` / `monitor_action.rs`, which is also the cheapest thing to
re-apply if the THE-75 lane conflicts (see §5).

Why an overlay rather than a `DetailOverlay` or a panel tab: the board needs a
two-band chrome (stage header rail + footer legend), horizontal cursor
movement across stage columns, and per-row hit-testing — the same three reasons
`monitor.rs:8-18` gives for not being a `DetailOverlay`. It does **not** need
`sections::Section`; a `Section` stack cannot express aligned columns across
group boundaries (that is the current bug). The board owns a
`Vec<BoardLine>` and paints with `seg::draw_line`, so it needs **no change to
`sections.rs`** and cannot regress the six surfaces that share it.

Render-decision purity is satisfied for free: `render_plan::Overlays::layers`
(`render_plan.rs:88-99`) is derived by `layer::open_layer` via
`caret::no_covers()`, so a new boxed layer forces `Full` without anyone editing
`Overlays`. The board is chrome; every board change is a `Full` frame.

### D2 — No new wake source

The board reuses the existing roster refresh verbatim. `run.rs:9571-9588`
already gates a one-shot `spawn_dispatch_sample` on
`monitor.wants_dispatches()`; the gate's expression becomes
`board.wants_dispatches()`. Same `DISPATCH_SAMPLE_EVERY` cadence, same
`RefreshKind::Dispatches` apply at `run.rs:10487`, same event-driven half
(`monitor_pipeline::take_roster_dirty`, `note_roster` at
`attention_status.rs:196`). Nothing new is spawned, timed or subscribed.

The sidebar lane folders (§4) ride the _other_ existing door: the roster is
already read off-loop in `attention_status.rs:194` and folded three ways; the
lane fold is a fourth derivation over rows already in memory.

### D3 — Time is normalized in core, at the read seam, plus a migration

Two independent defences, because either alone is wrong:

1. **Migration** (`db.rs`) at `SCHEMA_VERSION` 57 → **58**, following the
   `ver < 46` / `ver < 57` precedent at `db.rs:875-893`: one idempotent
   `UPDATE … SET dispatched_at_ms = dispatched_at_ms * 1000 WHERE
dispatched_at_ms > 0 AND dispatched_at_ms < 100000000000`, gated on the
   pre-bump on-disk version so it runs once, stamped before `db.rs:899`
   writes the new `user_version`.
2. **Read-side guard**, pure and tested, in `thegn_core::issue`, applied in
   the single row mapper at `db_notification.rs:463`. A DB that a _newer_
   build wrote, a JSON roster deserialized over the control API, a hand-edited
   row — none of those go through the migration, and none of them may render
   as decades.

The threshold is `1e11`. As milliseconds that is 1973-03-03; as seconds it is
the year 5138. So "below 1e11" is unambiguously a seconds value for any
timestamp this program can legitimately hold, in both directions, for the next
three thousand years. `<= 0` is left alone (an unstamped row must read as
unstamped, not as 1970 × 1000).

`fmt_age_ms` (`monitor_pipeline.rs:240-251`) keeps its negative clamp; it is
the last line of defence, not the fix.

### D4 — Left-to-right is a _layout mode_, with a tested degradation

`layout::board(...)` is pure: `(rows, stages, width, now_ms) -> Board`.

- **Columns mode** when `width >= min_col_w * n_columns` (`min_col_w = 22`).
  One column per stage in `[[pipeline.stages]]` declaration order, then stages
  found only on the roster (by name), then `unstaged` — the same precedence
  `ordered_rows` already implements (`monitor_pipeline.rs:129-145`), so the two
  can never disagree about order.
- **Stacked mode** below that width: strong stage headers with counts, rows
  beneath — i.e. today's shape, but with the header band's facts, the legend
  and the cues. The directive allows either; the point is that the fallback is
  a _tested pure decision_, not an accident of terminal size.

Edges:

- The header band draws the org chart from each stage's `next`
  (`config_pipeline.rs:70`): `architect ──▶ code ──▶ review`, using
  `GlyphSet::box_h` and a new `GlyphSet::arrow_right`. The rail is **config**,
  not inference.
- A row whose `parent_id` resolves to a row in the **previous column** is
  prefixed with an inbound-edge mark; the parent carries an outbound tick. A
  row whose parent is in the **same** column (a chunk fan-out) keeps
  `ordered_rows`' depth and draws `tree_tee` / `tree_corner` connectors —
  `GlyphSet` fields that already exist (`termcaps.rs:385-386`).
- Rows with no edge get a leading space so column alignment is exact. The
  ratchet (`test/glyph-literal-ratchet.txt`) forbids U+2500–U+259F literals
  outside the caps chokepoints, so every connector comes from `active_glyphs()`.

### D5 — Everything the issue lists as missing has a named home

| Gap                                   | Where it lands                                                                                                                                                                                                                                      |
| ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| flat grouped rows read poorly         | D4 columns                                                                                                                                                                                                                                          |
| chunk parent-indent too subtle        | D4 connectors + edge marks                                                                                                                                                                                                                          |
| nothing looks clickable               | selected row painted with `S::Accent` + a cursor bar (`GlyphSet::half_block_r`), hover-free but hit-tested (`handlers/overlay.rs` arm)                                                                                                              |
| no footer legend                      | a footer `Line`, always drawn, listing **every** bound key                                                                                                                                                                                          |
| stalled/timeout cues                  | pure `is_stalled(row, timeout_secs, now_ms)`: active **and** age > `stage.timeout_secs × 1000` ⇒ `warn` glyph + `Hue::Amber`. `timeout_secs` is advisory config (`config_pipeline.rs:66-69`) — the board _displays_ the breach, it never acts on it |
| configured-but-empty stages invisible | the column/header is emitted for every configured stage regardless of row count, with a dim `—` placeholder                                                                                                                                         |
| concurrency/agent/next not shown      | stage header: `code · claude · 2/4 · → review`                                                                                                                                                                                                      |
| per-stage tables break alignment      | no `TableSection` at all; one column grid for the whole board                                                                                                                                                                                       |
| status glyphs collide, bypass caps    | D6                                                                                                                                                                                                                                                  |
| Enter should open a non-tab worktree  | D7                                                                                                                                                                                                                                                  |

### D6 — Status glyphs go through the caps ladder, and stop colliding

`AgentDispatchStatus::glyph()` (`issue.rs:384`) stays (the CLI prints it) and
gains a sibling `glyph_set(&GlyphSet) -> (&'static str, Hue)` — the exact
shape `attention.rs:394` and `notification.rs:311` already use, so this is a
third instance of an established pattern, not a new one. Collisions are
resolved by giving `Queued`, `Spawning` and `Running` distinct marks
(`diamond_hollow` / `refresh` / `dot_filled` — all existing `GlyphSet`
fields). `WaitingHuman` uses `attention`, `PrOpen` uses `hex`, terminals use
`check` / `cross`. The only genuinely new `GlyphSet` field is
`arrow_right` (`→` / `>`).

`GlyphSet` is in `thegn-core` and every field is asserted BMP + width-1 +
ASCII-in-`ASCII` by existing tests (`termcaps.rs:341-343` policy note); the new
field must satisfy those.

### D7 — Enter opens the worktree even when it is not a tab

`pipeline_target` (`monitor_action.rs:272-286`) resolves only against
`model.sidebar_rows` filtered to `RowKind::Worktree` with a `tab_target`. A
loaded workspace only produces rows for worktrees present in
`session.worktrees` (`sidebar.rs:1181-1201`); the dormant-workspace path
(`sidebar.rs:1204-1254`) synthesizes `RowTarget::Workspace`. So a registered
worktree that has never been opened in this session has no row, and the board
says `no open worktree for …` (`run.rs:13678`).

Fix, pure and testable: a second tier. If no sidebar row matches, look the
path up in `model.sidebar_db_worktrees` (`chrome.rs:458`) and return
`RowTarget::Workspace { repo_path, group: Some(tab_name) }` — the same target
the dormant path already builds, dispatched through the same
`handlers::sidebar_activate::activate_row_target` door (`run.rs:13652`). Only
when _both_ tiers miss does the board report a miss.

### D8 — Sidebar lanes are derived, never persisted

Each active lane gets a folder in the sidebar, under the existing PIPELINE
door row.

**Reuse the machinery, not the storage.** The `Merged` folder
(`merge_lifecycle.rs:76-84`, `config.merged_folder`) is a _real_ `folders` row
with a `folder_id` and a user-editable `position`. A lane is not: it must
appear and vanish with the roster. Persisting one would (a) fight the user's
own filing, (b) leave a dead folder behind every finished lane, (c) need a
reaper. So lanes reuse `SidebarRow` + `ViewState::collapsed` + `child_count` +
caret rendering — the folder _machinery_ — with no DB write anywhere.

Row shapes (all emitted at the **tail**, immediately under the existing
`RowKind::PipelineSummary` door at `sidebar.rs:1012-1024`; the tail placement
is load-bearing and its reason is documented there — the sidebar cursor is a
visible-row index, so a head placement shunts the cursor every time an agent
starts or finishes):

```
Pipeline ▸ 4 running            RowKind::PipelineSummary   (unchanged door)
  THE-74 · tg-the-74-…          RowKind::PipelineLane      collapsible
    architect · claude  ✓ 2h    RowKind::PipelineAgent     collapsible
      tg-the-74-pipeline-…      RowKind::PipelineWorktree  leaf, real target
    code · claude       ● 4m    RowKind::PipelineAgent
      tg-the-74-pipeline-…      RowKind::PipelineWorktree
```

Three **new** row kinds rather than `RowKind::Folder` + `RowKind::Worktree`,
for two concrete reasons found in the code:

1. `SidebarRow::is_markable` (`sidebar.rs:326-329`) and the mark set
   (`handlers/sidebar_keys.rs:904,922,969`) key on `pin_key`. A mirrored
   `RowKind::Worktree` row sharing a primary row's `pin_key` makes a bulk
   action count one worktree twice, and `sidebar_keys.rs:544`'s
   `.position(|r| r.pin_key == target_key)` becomes ambiguous. This is exactly
   the identity-anchor / pin-key trap the sidebar audit records.
2. `pipeline_target` (D7) searches `sidebar_rows` for the _first_
   `RowKind::Worktree` with a matching path. A mirror row of that kind makes
   the board's own jump depend on emission order.

`RowKind::PipelineWorktree` keeps the **hit-target** identity that matters —
`worktree_path` and the real `tab_target`, so `↵` and a click land exactly
where the primary row's would (`sidebar_keys.rs:639`, `sidebar_mouse.rs:284`)
— while carrying an empty `pin_key`, which every pin/mark path already skips
(`sidebar_keys.rs:303,347,424`). Lane and agent rows carry a `pipeline/…`
`pin_key` _purely_ as a collapse key (`collapse_key()` gains the two arms;
`sidebar.rs:331-338` is the precedent — folders already key collapse on
`pin_key`).

Lane identity and name, both pure:

- key: the roster row's `issue_id` when non-empty, else the earliest root
  row's worktree basename.
- label: `{issue_id} · {basename}`, or just the basename when there is no
  issue id — "the issue id + short title, derived from the roster's
  issue_id/worktree".
- a lane exists **only while it has active rows** (`status.is_active()`,
  `issue.rs:377-382`), matching the door row's own gate
  (`sidebar.rs:1018`).

Ordering: lanes by their earliest active `dispatched_at_ms` (oldest lane
first — the order work started, the same reading `ordered_rows` uses); agents
within a lane by configured stage order then `dispatched_at_ms`.

### D9 — No new config keys

Everything the board displays already exists as config
(`[[pipeline.stages]]`'s `name` / `agent` / `concurrency` / `timeout_secs` /
`next`, documented at `config/config.toml.example:1487-1560`). The two runtime
toggles (freeze, hide-finished) are per-open view state and are deliberately
**not** persisted — the board is a live view, and a hidden-by-default row set
that survives a restart is a support ticket. So `config.toml.example` needs no
new key, and the "config keys documented" invariant is satisfied vacuously.
Any coder who finds they _need_ a key must add it to `config.toml.example` in
the same commit.

---

## 2. Invariant checklist

| Invariant                               | How this change honours it                                                                                                                                                                                               |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 0 % idle                                | D2 — no timer, no thread, no subscription added. The board's sampler is the monitor tab's, moved.                                                                                                                        |
| Render decision pure                    | Board is a `layer::open_layer` box ⇒ `Overlays::layers` ⇒ `Full`. `render_plan.rs` needs no edit; its existing test `an_open_board_takes_the_overlay_rule_like_every_other_modal` (`render_plan.rs:245`) keeps passing.  |
| Degrade at the edges                    | Every glyph from `caps::active_glyphs()`; no color literal (all `Tok::Slot` / `Tok::Hue`). `test/glyph-literal-ratchet.txt` must not gain an entry.                                                                      |
| `thegn-core` substrate-free, 95 % lines | Chunk 1's additions are pure functions with unit tests in-crate. No new dependency.                                                                                                                                      |
| Seams, not vendors                      | Nothing new; the roster is already the seam.                                                                                                                                                                             |
| One capability catalog                  | No new capability — `open-pipeline-board` already exists (`keymap.rs:562`).                                                                                                                                              |
| git is truth, SQLite a cache            | The migration rewrites a cache column in place; a fresh DB matches zero rows.                                                                                                                                            |
| God-files don't grow                    | New code goes in `pipeline_board/` and `sidebar_pipeline.rs`. `run.rs` gains only wiring.                                                                                                                                |
| Help ratchets                           | No new action id, so `test/help-ratchet.txt` is unchanged. `open-pipeline-board`'s claim **moves** from `system-monitor.md` to a new `pipeline-board.md`, which must mention `Alt b` by chord (the prose ratchet).       |
| Ignored `Result`s                       | The migration `UPDATE` is `let _ =` with the same `// best-effort:` reasoning as `db.rs:876,890`.                                                                                                                        |
| e2e                                     | Not run in this lane. The board is an overlay and appears in no baseline; the sidebar lane rows show a relative age, so if a baseline is ever re-recorded that age must be pinned in `e2e_freeze.rs`. Flagged, not done. |

---

## 3. Chunks

| #   | Scope                                                            | Files                                                                                    | Depends on                                |
| --- | ---------------------------------------------------------------- | ---------------------------------------------------------------------------------------- | ----------------------------------------- |
| 1   | core: time normalization + migration + caps-ladder status glyphs | `thegn-core` only                                                                        | —                                         |
| 2   | the board surface                                                | `pipeline_board/*`, `monitor*`, `run.rs`, `handlers/overlay.rs`, `keymap_specs.rs`, help | **1** (needs `glyph_set` + `arrow_right`) |
| 3   | sidebar lane folders                                             | `sidebar*`, `attention_status.rs`, `handlers/sidebar_*`, help                            | —                                         |

**Parallelism.** Chunks **1 and 3 are file-disjoint and may run in parallel.**
Chunk 2 must start after chunk 1 lands (it calls `glyph_set` and reads
`GlyphSet::arrow_right`). Chunk 2 and chunk 3 are file-disjoint from each
other and may run in parallel once 1 is in.

The one file both host chunks could reach for is
`crates/thegn-host/src/monitor_pipeline.rs`. It belongs to **chunk 2 only**;
chunk 3 puts its fold in a new `sidebar_pipeline.rs`. Likewise
`docs/help/`: chunk 2 owns `pipeline-board.md` + `system-monitor.md` +
`help/pages.rs`; chunk 3 owns `sidebar.md`.

---

## 4. Risks

- **`SCHEMA_VERSION` collision.** Chunk 1 takes 58. Another lane bumping to 58
  concurrently is the known conflict class; on merge, take the higher number
  and re-key the `ver <` guard to match. The guard is idempotent, so a
  re-numbered migration is still correct.
- **THE-75 merge.** That lane edits `monitor.rs` for the other tabs. Chunk 2
  deletes `MonitorTab::Pipeline` from the same file. Mitigation is procedural
  and binding: chunk 2 does the monitor deletion as its **own final commit**,
  touching nothing else, so a conflicted merge is re-applied by re-running one
  small deletion rather than untangling a feature commit.
- **Board width.** The columns/stacked decision must be a pure function with
  tests at the boundary, or a narrow terminal silently produces a board with
  one-cell columns.

## 5. Out of scope

Pane-level focus (jumping to the _session_ running a stage rather than its
worktree) stays phase 2; `PipelineJump::session` keeps riding the request
unused, as `monitor.rs:92-96` already documents. Nothing in this lane advances
a stage, enforces `concurrency` or fires `timeout_secs` — the doctrine at
`monitor_pipeline.rs:9-15` and `config_pipeline.rs:5-16` stands: thegn
validates and displays; the Lead judges.
