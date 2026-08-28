# Design — fix-drag-grab-precision

Full audit with citations lives in the branch's architect record
(`.thegn/pipeline/THE-67/architect/design.md`). This file records what chunk 2
shipped and why, and owns the pane-frame behavior (F4/F5), which has no
capability spec.

## The drag model

One rule set, applied to every surface here:

1. **The grab target is at least as wide as the thing looks.** A 1-column
   divider drawn next to a 1-column pane border reads as one boundary; both
   columns grab. A row's drop target is the whole row.
2. **Press arms; motion commits.** A press inside a grab band must not mutate
   anything by itself. The gesture becomes real on the first pointer sample
   that moved; a release without motion is a plain click on whatever was
   under it.
3. **The grabbed thing stays under the cursor.** The offset between the press
   column and the separator is held for the whole drag (`sep_follow`).
4. **Esc cancels every drag**, restoring the pre-drag state, never
   half-applied.
5. **A widened band never steals a live click.** The extra cells come out of
   chrome furniture, not out of list rows or pane content; where that cannot
   be proven statically, the widening is conditional on the cell being
   furniture at that moment.
6. **The hit test is pure and unit tested**, and drag feedback stays a chrome
   change → `RenderPlan::Full`.

## The separator band

The band is `{sep, sep + 1}` for the sidebar and `{sep - 1, sep}` for the
panel — the extra cell always comes from the **center** column's outer frame
cell, the second vertical rule at that boundary. The center side is chosen so
a widened band can never cost a sidebar row or panel row activation (rule 5):
both lists are live click targets across their full width.

The extra cell is still gated at the call site: the non-full-width bottom
drawer is laid out at `x = center_x`, so in the drawer's rows the band's extra
cell is drawer content. The grab requires
`sep_is_exact(sep, mx) || hit_pane.is_none()` — the separator column always
grabs; the furniture cell only when it is not pane/drawer content. `hit_pane`
is precisely "the pointer is inside a pane's or the drawer's content rect".

## Press arms, motion commits

The loop's two `bool` drag flags became
`Option<(press_x, sep, moved, width_snapshot)>` per separator:

- **Grab** records the tuple, shows the hint status, sets `mouse_left_down`.
  Nothing else mutates — in particular `sb.collapse_wide()` moved off the
  press.
- **Motion** at the press column does nothing at all (the threshold). The
  first sample that moves sets `moved` and, for the sidebar only, does the
  Wide drop-out that used to happen on press — the same reason a `<`/`>`
  nudge drops out: the width you dragged to is the width you get. Width is
  computed from `sep_follow(press_x, sep, mx)` (sidebar width **is** the
  separator column; panel width is `cols - sep_follow(..) - 1`) through the
  **unchanged** clamp expressions, and recomputes chrome (a chrome change →
  `RenderPlan::Full`).
- **Release with motion** persists off-loop through the same `db_task::persist`
  calls as before and reports the settled width. **Release without motion**
  clears the grab, persists nothing, reports no width: a bare click on the
  divider is a no-op.
- **Esc** extends the existing gesture-cancel arm (the one that cleared
  `pane_lift` / `pane_border_grab`): a separator grab is cancelled by
  restoring the snapshotted width (only when `moved` — otherwise nothing
  changed), re-applying the panel width via `layout::set_panel_width_cfg`,
  recomputing chrome, and persisting nothing.

## The pane frame (F4/F5) — no owning capability spec

The pane gestures live in the center's `CenterTree`, which has no capability
spec of its own (the sidebar and panel do). This change documents the shipped
behavior here and in `docs/help/terminal-and-panes.md` rather than inventing a
capability:

- **A press on a pane frame (title bar / outer edge) arms a rearrange lift and
  does not reach focus dispatch.** That was the bug: the lift `continue`d
  past the focus path, so a motionless release — which commits nothing —
  swallowed the click. Now `pane_lift` carries `(pane, press_x, press_y,
moved)`; a release with `moved == false` is a **click on the frame** and
  focuses the pane with the same two lines the content-click path uses
  (`focus.zone = Center`, `tab.focused_pane = id`). With motion, the release
  swaps / re-anchors / persists exactly as before.
- **The seam-resize grab band widens by `[theme] pane_padding`.**
  `border_at(frames, mx, my, slop)` widens the seam band `slop` columns past
  the two border cells on each side, both axes. The call site passes
  `crate::center::PANE_HPAD` — `inset` reserves the pad cells as frame, so
  the visible gutter is `2 + 2·pad` wide and the pad cells read as part of
  it; without the slop they fell through to the rearrange lift. The content
  early-return runs **first**, so a pointer inside a content rect reaches the
  pane app before the seam scan regardless of slop — no slop value can steal
  from a pane app (asserted in `pane_drag.rs` tests at slop 3).

## Invariants held

- Render decision: every new path routes through the existing `dirty` /
  `need_relayout` / `sidebar_dirty` flags; nothing sets `selection_only`.
- 0% idle: no timers, no wake sources; every step is driven by an inbound
  mouse event; `drain_drag_events` coalescing untouched.
- No new ignored `Result`s, no color/glyph literals, no platform `cfg`, no new
  `ACTION_SPECS` ids (help ratchets do not move; prose updated to match).
- `run.rs` gets call-site edits and loop-local tuple state only; the geometry
  lives in `drag_hit.rs` / `pane_drag.rs`, unit tested at the module. The
  `#[expect(dead_code)]` wiring gates chunk 1 put on the `drag_hit` API come
  off in this change, which is its first non-test caller.
