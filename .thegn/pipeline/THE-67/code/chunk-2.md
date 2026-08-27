# Chunk 2 — Loop wiring: the separator grab, the pane gestures, docs + openspec

**Issue:** THE-67 (drag/drop needs unnecessary mouse precision).
**Design:** `.thegn/pipeline/THE-67/architect/design.md` — read all of §1, §2.2,
§2.3, §3.1, §3.2 and §4 before starting.
**Runs:** **after chunk 1** — it calls `crate::drag_hit::{SepSide, sep_grab,
sep_is_exact, sep_follow}`, which chunk 1 creates. Independent of chunk 3 (no
shared files), but land it last: it owns every prose artifact and therefore
documents all three chunks' behavior.

## Why

Four defects, all in the event loop's mouse arm:

- **F1** — the sidebar and panel width drags grab on **one column**:
  `chrome.sep_left == Some(mx)` (`run.rs:12623`) and `chrome.sep_right ==
Some(mx)` (`run.rs:12639`). The separator is one column by construction
  (`layout.rs:554,562`), and the column right next to it is the leftmost pane's
  **frame border** (`center_x = left + sep_left_w`, `layout.rs:564`; panes
  reserve a 1-cell ring, `center.rs:231-246`). The user sees two adjacent
  vertical rules and can only aim at "the boundary"; half of it is dead.
- **F2** — the sidebar grab mutates on the **press**: `sb.collapse_wide()` plus
  a status rewrite at `run.rs:12626-12634`, before any motion. A stray click on
  the divider drops the sidebar out of its Wide expand, irreversibly.
- **F3** — the Esc-cancels-a-drag rule covers `pane_lift` / `pane_border_grab`
  (`run.rs:13886-13895`) and the sidebar row drag (`run.rs:13913-13922`), but
  **not** the two separator drags. They can only be ended by releasing.
- **F4** — a press on a pane frame lifts a rearrange and `continue`s
  (`run.rs:12662-12674`), so the press never reaches the focus dispatch at
  `run.rs:12865-12875`: **clicking a pane's title bar does not focus the pane**.
  Release with no motion resolves to `DropTarget::None` (`pane_drag.rs:154`) and
  commits nothing, so the click is simply swallowed.
- **F5** — `border_at`'s seam band is a fixed 2 columns (`pane_drag.rs:50,68`)
  but `inset` insets content by `1 + PANE_HPAD` (`center.rs:231-239`, fed by
  `[theme] pane_padding`, `run.rs:709-712`). With padding the visible gutter is
  `2 + 2·pad` columns while only 2 grab; the pad cells fall through to the lift
  branch and start a **rearrange** where the user aimed at a resize.

## Files you own

- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/pane_drag.rs`
- `docs/help/sidebar.md`, `docs/help/panel.md`, `docs/help/terminal-and-panes.md`
- `openspec/changes/<your-change-name>/**` (new)

Do not touch `drag_hit.rs`, `chrome.rs`, `chrome_tests.rs`, `main.rs`,
`sidebar_view.rs`, or `handlers/sidebar_mouse.rs` — chunks 1 and 3 own those.
Do **not** hand-edit `openspec/specs/**`: main specs are synced from a change
folder, never written directly.

## Approach

### 1. `pane_drag.rs` — pad-aware seam slop (F5)

Give `border_at` a `slop: usize` parameter. The seam band widens by `slop` on
each side of the two border columns:

- vertical: `on_seam` becomes `mx + 1 + slop >= rf.x && mx <= rf.x + slop`
  (equivalently, `mx` in `rf.x - 1 - slop ..= rf.x + slop`, saturating);
- horizontal: the same on `my` against `rf.y` (`pane_drag.rs:68`).

Leave the content early-return (`pane_drag.rs:37-39`) exactly where it is — it
is what makes any slop safe: a pointer inside a content rect returns `None`
before the seam scan runs, so no slop value can ever steal from a pane app.

Update the doc comment to say the slop is `[theme] pane_padding`, and why: the
pad cells are frame cells that read as part of the gutter, and without the slop
they lift a rearrange instead of resizing.

Tests in `pane_drag.rs`'s module (`pane_drag.rs:179`):

- `slop = 0` reproduces every existing assertion — keep the current tests
  passing by threading `0` through them, do not delete them;
- with a pad-widened layout, a cell one column outside the two border cells is
  a hit at `slop = 1` and a miss at `slop = 0`;
- a content cell is `None` at any slop (assert at `slop = 3`).

### 2. `run.rs` — separator grab state (F1, F2, F3)

Replace the two `bool`s at `run.rs:6323` / `:6327` with state that carries the
press column and whether the drag has actually moved. Keep it local to the loop,
tuples or a small local struct — **no new helper module** (`run.rs` is
ratchet-pinned; new logic belongs in `drag_hit.rs`, which chunk 1 already
provides):

```rust
// (press column, separator column at press, moved yet?, width to restore on Esc)
let mut sidebar_sep_grab: Option<(usize, usize, bool, Option<usize>)> = None;
let mut panel_sep_grab:   Option<(usize, usize, bool, Option<usize>)> = None;
```

Snapshot `sb.width` / `panel_cols_pref` at grab time for Esc; keep the existing
comments' content (what each drag persists and where) in the new declarations.

**Grab (replacing `run.rs:12623-12646`).** For the sidebar:

```
left && !mouse_left_down && !model.sidebar_rail
  && crate::drag_hit::sep_grab(chrome.sep_left, SepSide::Sidebar, mx)
  && (crate::drag_hit::sep_is_exact(chrome.sep_left, mx) || hit_pane.is_none())
```

and the mirror for the panel with `SepSide::Panel`, `chrome.sep_right`, keeping
the existing `panel_ui.width == layout::PanelWidth::Normal` guard.

The `hit_pane.is_none()` clause is load-bearing, not defensive: the
non-full-width bottom drawer is laid out at `x = center_x`
(`layout.rs:632-637`), i.e. its content occupies the band's extra cell in the
drawer's rows, and `hit_pane` is precisely "the pointer is inside a pane's or
the drawer's content rect" (`handlers/overlay.rs:186-197`). The separator column
itself always grabs. Say that in a comment.

On grab: record `(mx, sep, false, snapshot)`, set `mouse_left_down = true`, show
the existing hint status, `dirty = true`, `continue`. **Do nothing else** —
in particular do **not** call `sb.collapse_wide()` here (F2).

**Motion (replacing `run.rs:12552-12578` and `:12586-12616`).** While grabbed
and `left`:

- if the pointer column still equals the press column, do nothing at all (this
  is the threshold — a press that has not moved is not yet a drag);
- on the first sample that moved, set `moved = true` and, for the sidebar only,
  do the Wide drop-out that used to happen on press (`sb.expanded` →
  `sb.collapse_wide()`, recompute chrome, `need_relayout = true`) — the comment
  at `run.rs:12617-12622` explaining why a drag drops out of the expand still
  applies, just later;
- compute the new width from `sep_follow(press_x, sep_at_press, mx)` so the
  divider keeps its grab offset: sidebar width **is** the separator column
  (`layout.rs:554`), and panel width is `cols - sep_follow(..) - 1` (mirroring
  today's `cols.saturating_sub(mx + 1)` at `run.rs:12554`). **Clamps are
  unchanged** — reuse the existing `clamp(30, (cols/2).max(30))` and
  `clamp(SIDEBAR_MIN_WIDTH, sidebar_max_width())` expressions verbatim.

**Release.** Unchanged when `moved` — persist off-loop through the same
`db_task::persist` calls and report the settled width. When `!moved`: clear the
grab, `mouse_left_down = false`, `dirty = true`, and **persist nothing and
report no width** — a bare click on the divider is now a no-op. (The comment at
`run.rs:12609-12612` already anticipated this case; delete or rewrite it to
match.)

**Esc (F3).** Extend the existing gesture-cancel arm at `run.rs:13886` — the one
that already clears `pane_lift` / `pane_border_grab` — to also cancel a
separator grab: restore the snapshotted `sb.width` / `panel_cols_pref` (only
when `moved`; nothing changed otherwise), re-apply the panel width through
`layout::set_panel_width_cfg` the same way the motion arm does, recompute
`sidebar_cols` / `chrome`, set `need_relayout` + `dirty`, persist **nothing**.
Keep it in that one arm rather than adding a second Esc branch.

### 3. `run.rs` — pane gestures (F4, F5)

- The `border_at` call site (`run.rs:12654`) passes
  `crate::center::PANE_HPAD.load(std::sync::atomic::Ordering::Relaxed)` as the
  slop.
- `pane_lift` (`run.rs:6336`) becomes `Option<(PaneId, usize, usize, bool)>` —
  id, press column, press row, moved. The motion arm (`run.rs:12519-12523`) sets
  `moved` once the pointer leaves the press cell.
- The release arm (`run.rs:12524-12544`): when `moved` is false, skip the
  swap/anchor entirely and **focus the lifted pane** instead — the same two
  lines the content-click path uses (`run.rs:12872-12875`):
  `focus.zone = crate::focus::Zone::Center;` and
  `session.active_tab_mut()` → `tab.focused_pane = id`. Clear the status hint.
  When `moved` is true, behave exactly as today.
- Comment it as the drag model's rule 2 applied to panes: a lift that never
  moved is a click, and a click on a pane focuses it.

### 4. Invariants to hold while editing `run.rs`

- Every new path routes through the existing `dirty` / `need_relayout` /
  `sidebar_dirty` / `bars_dirty` flags. Drag feedback is a chrome change →
  `RenderPlan::Full` (`render_plan.rs:139-141`).
- **Never set `selection_only`** (`run.rs:11486-11493`) on any of these paths —
  that fast path is for pane text selection and skips chrome recomposition.
- No timers, no new wake sources, no polling; every step is driven by an inbound
  mouse event. The `drain_drag_events` coalescing (`run.rs:4556`) is untouched.
- No new ignored `Result`s, no color/glyph literals, no platform `cfg`.

### 5. Help pages

No new `ACTION_SPECS` action ids are added, so the help ratchets do not move —
but the prose must match the shipped behavior. Update:

- `docs/help/sidebar.md` — the drag bullet at ~L91: releasing anywhere in the
  sidebar lands on the nearest row (chunk 3), and the width section at ~L179:
  the separator grabs on the divider **or the pane edge beside it**, a click
  that does not move changes nothing, and `Esc` cancels a resize.
- `docs/help/panel.md` — the "Width: memory, DWIM, config, drag" section at
  ~L108: same two-column grab, same click/Esc semantics.
- `docs/help/terminal-and-panes.md` — a click on a pane's frame/title focuses
  that pane; a drag from it rearranges; `Esc` cancels.

Keep the existing voice, do not add a new section where a sentence fits, and do
not touch the generated keybindings/config-reference pages.

### 6. OpenSpec change

Create `openspec/changes/<kebab-name>/` (e.g. `fix-drag-grab-precision`) with
`proposal.md`, `design.md`, `tasks.md` and delta specs, following
`openspec/config.yaml`'s schema. Model it on a recent well-formed change —
`openspec/changes/add-pipeline-board/` — and cite THE-67 plus the `tasks.md`
roadmap group in the proposal's Impact.

Delta specs, using `## MODIFIED Requirements` / `## ADDED Requirements`:

- `specs/sidebar/spec.md` — **MODIFIED** "Configurable, resizable sidebar width"
  (`openspec/specs/sidebar/spec.md:190-224`): the drag grabs a two-column band
  (the separator plus the adjacent pane frame cell, the latter only when it is
  not pane/drawer content), a press that never moves changes nothing, `Esc`
  cancels restoring the pre-drag width, and the divider holds its grab offset.
  Add a scenario per new behavior. The existing "Rail refuses a resize" scenario
  is unaffected — keep it. Plus an **ADDED** requirement for the drag drop
  target: a release anywhere inside the sidebar's rect resolves to the nearest
  row (chunk 3), while a release outside it cancels.
- `specs/panel/spec.md` — **ADDED** requirement for the panel separator's grab
  band, mirroring the sidebar's wording.

The pane-frame behavior (F4/F5) has no owning capability spec — document it in
the change's own `design.md` and in `docs/help/terminal-and-panes.md`; do not
invent a capability for it.

Validate with `just openspec-validate` (`openspec validate --all --strict`).
Do **not** run `/opsx:sync` or archive the change.

## Tests to run (scoped — no full-workspace gates)

```sh
just quick thegn-host
cargo nextest run -p thegn-host pane_drag
cargo nextest run -p thegn-host border_at
cargo nextest run -p thegn-host drop_on
just openspec-validate
```

Do **not** run `just test`, `just ci`, `just coverage`, or `just e2e` — this
change alters frames, so e2e baselines are stale by construction and re-recording
is a separate, deliberate step.

## Done criteria

- Both separators grab on a two-column band; the extra cell is skipped when
  `hit_pane` is `Some` (drawer/pane content).
- A press on either separator that never moves: no width change, no
  `collapse_wide`, no persist, no width report.
- A moved drag keeps the divider under the cursor (`sep_follow`), with the
  existing clamps and persistence untouched.
- `Esc` cancels either separator drag and restores the pre-drag width.
- `border_at` takes a slop, the call site passes `PANE_HPAD`, and its tests cover
  slop 0 (unchanged behavior) and slop > 0.
- A pane lift released without motion focuses the pane; with motion it swaps /
  re-anchors as before.
- The three help pages describe the shipped behavior; `just openspec-validate`
  passes.
- `just quick thegn-host` is clean.

**Commit subject (exact):**

```
fix(the-67): forgiving separator + pane drag grabs, Esc cancels
```
