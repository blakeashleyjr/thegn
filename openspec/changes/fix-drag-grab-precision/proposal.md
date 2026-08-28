# Fix drag grab precision — forgiving separator + pane drag grabs, Esc cancels

> Issue **THE-67** — "drag and drop is a bit finicky. It works well but the
> selection with the mouse requires unnecessary precision." Design record:
> branch `tg/the-67-drag-precision`, chunks 1–3.

## Why

Auditing every mouse-drag surface in the compositor against the code that
paints it (see the change's `design.md`, which is also the branch's architect
record) found five defects, all in the event loop's mouse arm:

1. **The separator grab target is one terminal column.** The sidebar and panel
   width drags grab only when the pointer column equals the separator column —
   but the separator is painted one column wide, sits next to the leftmost
   pane's frame border, and the two adjacent rules read as one boundary. Half
   of what looks grabbable is dead.
2. **A press alone mutates the sidebar.** Pressing the sidebar separator
   dropped the sidebar out of its Wide expand and rewrote the status line
   before any motion — a stray click irreversibly left the expand.
3. **`Esc` did not cancel the separator drags.** The Esc-cancels-a-drag rule
   covered the pane gestures and the sidebar row drag but not the two width
   drags; they could only be ended by releasing.
4. **Clicking a pane's title bar did not focus it.** A press on a pane frame
   lifted a rearrange drag and skipped the focus dispatch, and a motionless
   release committed nothing — the click was swallowed.
5. **The pane seam resize ignored `[theme] pane_padding`.** The grab band was
   a fixed 2 columns while the visible gutter is `2 + 2·pad` wide, so the pad
   cells lifted a **rearrange** where the user aimed at a **resize**.

## What Changes

1. **Two-column separator grab bands** (`drag_hit.rs`, chunk 1): the band is
   the separator column plus the center column's outer frame cell beside it,
   taken from chrome furniture, never from a list row or pane content. The
   extra cell is skipped when it is pane/drawer content (`hit_pane.is_some()`),
   and the separator column itself always grabs.
2. **Press arms, motion commits** (`run.rs`, chunk 2): a separator press
   mutates nothing; the gesture becomes real on the first pointer sample that
   moves (the Wide drop-out for the sidebar happens then, like a `<`/`>` nudge);
   a release that never moved is a plain click that persists nothing and
   reports no width.
3. **The divider holds its grab offset** (`sep_follow`): the separator tracks
   the cursor instead of jumping to it on the first sample. Clamps and the
   release-time persistence paths are unchanged.
4. **`Esc` cancels every mouse drag** (`run.rs`, chunk 2): the existing
   gesture-cancel arm also cancels a separator drag, restoring the snapshotted
   pre-drag width (only when the drag moved) and persisting nothing.
5. **Pad-aware pane seam slop** (`pane_drag.rs`, chunk 2): `border_at` takes a
   `slop` and the call site passes `[theme] pane_padding`
   (`crate::center::PANE_HPAD`), so the pad cells beside a seam resize the pane
   instead of lifting a rearrange. The content early-return makes any slop
   safe — a pointer inside a content rect still reaches the pane app first.
6. **Click-to-focus on a pane frame** (`run.rs`, chunk 2): a lift released
   without motion focuses the lifted pane (the same focus the content-click
   path applies) instead of swallowing the click; with motion it swaps or
   re-anchors exactly as before.
7. **Sidebar row-drag drop target** (`sidebar_view.rs` +
   `handlers/sidebar_mouse.rs`, chunk 3): a release anywhere inside the
   sidebar's rect resolves to the nearest row — the blank tail below the list
   lands at the end — and a release outside the sidebar cancels.

## Impact

- **THE-67.** tasks.md roadmap: group **B (Workspace bar / tree)** — items 22
  (manual reorder) and 25 (adjustable bar width) — and group **G (Panes &
  layouts)** — item 98 (swap/drag-onto-center). All three were `[x]` but the
  gestures needed precision this change removes.
- **Spec deltas:** `sidebar` — MODIFIED "Configurable, resizable sidebar width"
  plus an ADDED requirement for the row-drag drop target; `panel` — ADDED
  requirement for the separator grab band. The pane-frame behavior (items 5–6)
  has no owning capability spec; it is documented in this change's `design.md`
  and in `docs/help/terminal-and-panes.md`.
- **No new action, keybind, config key, or `ACTION_SPECS` id**, so the help
  ratchets do not move; the prose of `sidebar.md`, `panel.md` and
  `terminal-and-panes.md` is updated to match the shipped behavior.
- **No new crate, module, DB schema, wake source, or render path.** All new
  geometry is pure and unit tested in `drag_hit.rs` / `pane_drag.rs`; drag
  feedback stays a chrome change → `RenderPlan::Full`; nothing here touches
  `selection_only`.
