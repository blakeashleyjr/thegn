# THE-67 — Drag and drop is a bit finicky (mouse selection needs unnecessary precision)

Design record. Branch `tg/the-67-drag-precision`.
Issue: <https://linear.app/blakeashley/issue/THE-67>
Report (verbatim, no further body): _"drag and drop works but the mouse
selection requires unnecessary precision."_

---

## 0. TL;DR

I audited every mouse-drag surface in the compositor against the code that
paints it. **The sidebar's row drag — the surface the report most likely means
by "drag and drop" — is already correct**: its drop target is the full visual
extent of the row, resolved from the same `build_sidebar` pass the renderer
painted (§2.1). The precision problem is real, but it is in the surfaces
_around_ it:

| #      | Surface                    | Defect                                                                                                                                                                                 | Evidence                                            |
| ------ | -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------- |
| **F1** | Sidebar / panel width drag | The grab target is **one terminal column**, `mx` compared for equality against the separator column                                                                                    | `run.rs:12623`, `run.rs:12639`; `layout.rs:554,562` |
| **F2** | Sidebar width drag         | A **press alone** (no motion) drops the sidebar out of the Wide expand and eats the click                                                                                              | `run.rs:12623-12634`                                |
| **F3** | Both separator drags       | **Esc does not cancel** them — the Esc-cancels-a-drag rule covers only the pane gestures and the sidebar row drag                                                                      | `run.rs:13886-13895`, `13913-13922`                 |
| **F4** | Pane frame                 | A press on a pane's frame lifts a rearrange drag and `continue`s, so **clicking a pane's title bar does not focus it**                                                                 | `run.rs:12662-12674`                                |
| **F5** | Pane seam resize           | The grab band is a fixed 2 columns and **ignores `[theme] pane_padding`**, so with padding the visible gutter is wider than the grabbable one — the pad cells lift a rearrange instead | `pane_drag.rs:50,68`; `center.rs:231-239`           |
| **F6** | Sidebar row drag           | The blank tail below the last row is a **dead drop zone** (`Spot::Invalid`), although it is inside the sidebar's rect and looks like part of the list                                  | `sidebar_mouse.rs:551-557`                          |
| **F7** | Center tab strip           | Chips are laid out with a **1-column dead gap** between them; a click there resolves to no tab and nothing else claims it                                                              | `chrome.rs:858-865, 871-876`                        |

Everything below is that audit with citations, the drag model the fixes
converge on, and the three chunks that implement it.

---

## 1. The drag model

One rule, stated once, that every surface here is measured against.

1. **The grab target is at least as wide as the thing looks.** A 1-column
   divider drawn next to a 1-column pane border reads as one boundary; both
   columns must grab. A row's drop target is the whole row. A chip's hit span
   includes the padding drawn with it.
2. **Press arms; motion commits.** A press inside a grab band must not mutate
   anything by itself. The gesture becomes real on the first pointer sample
   that moved, and a release without motion is a plain click on whatever was
   under it.
3. **The grabbed thing stays under the cursor.** When a band is wider than one
   cell, the offset between the press column and the separator is preserved for
   the whole drag; the divider does not jump to the pointer on the first sample.
4. **Esc cancels every drag**, restoring the pre-drag state, and never
   half-applies.
5. **A widened band never steals a live click.** The extra cells come out of
   chrome furniture, not out of list rows or pane content. Where that cannot be
   proven statically, the widening is conditional on the cell being furniture at
   that moment.
6. **The hit test is pure and unit tested**, derived from the same pass that
   painted, and drag feedback stays a chrome change → `RenderPlan::Full`.

Rule 5 is the one that decides most of the design. It is why the separator band
grows into the **center column** and not into the sidebar/panel (§3.1) — a
grab band that costs you a row activation trades one precision complaint for
another.

---

## 2. The audit

### 2.1 Sidebar row drag — correct, do not "fix"

This is the surface the issue title points at, and it is the one place that
needs no threshold or hit-target work. For the record, so the coders do not
redesign it:

- **Drop target is the full row.** `row_at` (`sidebar_view.rs:1132`) tests
  `my >= h.y && my < h.y + h.height` and ignores `mx` entirely; a drag sample
  anywhere on the row's line resolves to that row, and `on_drag_move`
  (`run.rs:13330-13337`) passes only `my2`. The pointer can wander out of the
  sidebar horizontally without losing the target.
- **The hit table cannot drift from the paint.** `hit_rows`
  (`sidebar_view.rs:1096`) is built from the same `build_sidebar` pass the
  renderer used, and the gesture holds a `SidebarLayoutLock`
  (`sidebar_mouse.rs:246-256`) so a focus-driven reflow mid-drag cannot move the
  rows under the pointer.
- **There is deliberately no between-rows insertion band.** The issue text asks
  for "a generous between-rows insertion band"; the code has already considered
  and rejected it, with the reason written down at `sidebar_mouse.rs:559-571`:
  a row is one cell tall, so a top-half/bottom-half split has no sub-cell to
  resolve against — the previous rule split at `height.div_ceil(2)`, which for a
  1-cell row is always the top half, so every drop meant "insert before" and the
  end of a run had no anchor. The current model is **displacement**: the hovered
  row's slot is the destination. Under displacement an insertion band would be
  strictly worse — it would halve a target that is currently 100% live.
  **Do not reintroduce it.**
- **The start threshold is already "press + move beyond a small threshold".**
  `DragPhase::Pressed → Dragging` fires when the pointer leaves the pressed
  row's band, re-derived from the row's live placement each sample
  (`sidebar_mouse.rs:389-413`). Sub-row jitter stays a click.
- **Esc already cancels it** (`run.rs:13908-13922` → `cancel_drag`).

The one genuine gap is **F6**: `spot_at` (`sidebar_mouse.rs:551-557`) maps a
sample that hits no row to `Spot::Invalid`. Inside the sidebar rect that
happens in the blank tail below the last row, and above the first painted row —
regions that look like part of the list and are inside the surface the drag
owns. §3.3 closes it.

### 2.2 Separator drags — the headline defect

`layout.rs:554,562` puts each separator on exactly one column:

```rust
let sep_left = (left > 0).then_some(left);            // just right of the sidebar
let sep_right = (right > 0).then_some(panel_x - 1);   // just left of the panel
```

and the grab is an equality test on the pointer column:

```rust
// run.rs:12623
if left && !mouse_left_down && !model.sidebar_rail && chrome.sep_left == Some(mx) {
// run.rs:12639
    && chrome.sep_right == Some(mx)
```

A single cell, on a full-height divider, with mouse reports quantized to cells.
That is **F1**, and it is the literal reading of "requires unnecessary
precision".

It is worse than one cell wide would suggest, because of what sits next to it.
`center_x = left + sep_left_w` (`layout.rs:564`) and panes reserve a 1-cell
frame ring (`center.rs:231-239`, `layout_framed` at `center.rs:246`), so the
column immediately right of the sidebar separator is the leftmost pane's **left
border**. On screen the user sees two adjacent vertical rules and can only be
expected to aim at "the boundary". Half of the boundary is dead.

Two more defects ride along:

- **F2** — the press immediately calls `sb.collapse_wide()` and rewrites the
  status line (`run.rs:12626-12634`) before any motion. A stray click on the
  divider therefore drops the sidebar out of its Wide expand and is not
  undoable. The mirror release path (`run.rs:12609-12612`) even carries a
  comment acknowledging that a press which released without moving "never set
  one" — the no-motion case was known, and only the status text was fixed.
- **F3** — the Esc arm at `run.rs:13886` covers `pane_lift` and
  `pane_border_grab`; the sidebar row drag has its own at `run.rs:13913`.
  Neither knows about `sidebar_sep_dragging` / `panel_sep_dragging`, so a
  separator grab can only be ended by releasing, wherever the pointer is.

### 2.3 Pane gestures

`pane_drag::border_at` (`pane_drag.rs:35-80`) is otherwise a good citizen — it
returns `None` for any pointer inside a content rect (`pane_drag.rs:37-39`), so
it can never steal from a pane app, and it matches both border columns of a
seam (`mx + 1 == rf.x || mx == rf.x`, `pane_drag.rs:50`; the row mirror at
`:68`). Two columns is a usable target.

**F5**: that band is hard-coded at the two border cells, but `inset`
(`center.rs:231-239`) insets content by `1 + PANE_HPAD` columns, where
`PANE_HPAD` is `[theme] pane_padding` (`run.rs:709-712`, default `0`,
`config.rs:2627`). With padding configured, the visible gutter between two panes
is `2 + 2·pad` columns while only 2 grab; the pad cells fall through to the
pane-lift branch (`run.rs:12662`) and start a **rearrange** — a different,
destructive gesture — where the user aimed at a resize. The content guard makes
widening safe by construction: cells that are content return `None` before the
seam scan runs.

**F4**: `run.rs:12662-12674` lifts a pane whenever the press is on a frame cell
that is not content, then `continue`s. The press never reaches the focus
dispatch at `run.rs:12865-12875`, so **clicking a pane's title bar or border
does not focus that pane** — you must click into the content, which for a
mouse-reporting app is forwarded away instead. Release with no motion resolves
to `DropTarget::None` (`pane_drag.rs:154`) and commits nothing, so the click is
simply swallowed. This is rule 2 of the model applied to the pane surface: a
lift that never moved is a click, and a click on a pane focuses it.

### 2.4 Center tab strip and pin strip

`strip_chip_spans` (`chrome.rs:839-867`) emits `(x, w, index)` with
`w = width(title) + 2` and then advances `x += w + 1`. The `+ 1` is a spacing
column that belongs to no chip, and `center_tab_hit` (`chrome.rs:871-876`) is a
strict span test, so **every gap column between two tabs is dead** (**F7**).
The enclosing branch (`run.rs:13038`) claims the whole strip, so the click is
consumed and nothing happens. One dead column per ~5 is a real miss rate on a
strip of short tab labels.

The **pin strip** is clean: `build_pin_strip` (`chrome.rs:1075-1099`) pushes
chips back-to-back with their own padding and emits the hit spans from the same
build that paints (`pin_chip_hit`, `chrome.rs:1125-1134`). No gaps, no drift,
nothing to fix. Pins are click-to-summon only — there is no pin drag surface.

### 2.5 Investigated, deliberately unchanged

- **Tab reorder by drag does not exist.** `center_tab_hit` has exactly one call
  site (`run.rs:13040`) and it switches the active tab. Adding drag-to-reorder
  is a new feature, not a precision fix, and is out of scope for THE-67.
- **`dragdrop.rs`** is a complete, unit-tested, and entirely **unwired** model
  for dropping a file from the tree onto a pane (`#![allow(dead_code)]`, header
  comment at `dragdrop.rs:1-11`: "the mouse-seam wiring … is a focused
  follow-up"). It has no hit-testing to be imprecise about. Out of scope; worth
  its own issue.
- **Panel rail hit** (`chrome.rs:2054-2069`) has the same gap-between-spans
  shape as F7, but it is a click surface built by the panel frame builder, not a
  drag surface, and widening it means reasoning about the rail's separators.
  Deferred deliberately.
- **Sidebar autoscroll** (`sidebar_mouse.rs:445-459`): a 2-row band at each edge
  with an overshoot-proportional step. Correct as designed and documented; the
  proportionality is load-bearing given mode-1002 reporting plus event
  coalescing.
- **The sidebar's `caret_x` collapse cell** (`sidebar_mouse.rs:210`,
  `sidebar_view.rs:1110-1118`) is a single-column click target. It is a click
  affordance, not a drag one, and widening it eats row activations on the very
  rows it sits on. Left alone; noted here so the next audit does not re-find it.

---

## 3. The change

### 3.1 A widened, offset-preserving, cancellable separator grab

**Band.** The grab band for a separator at column `s` is `{s, s+1}` for the
sidebar separator and `{s-1, s}` for the panel separator — the separator plus
the **center column's** outer frame cell in both cases, which is precisely the
second vertical rule the user sees. Pure helper, unit tested:

```rust
// crates/thegn-host/src/drag_hit.rs
pub enum SepSide { Sidebar, Panel }
pub fn sep_grab(sep: Option<usize>, side: SepSide, mx: usize) -> bool;
```

**Why not the other side.** Column `left - 1` is the sidebar's last column and
`panel_x` is the panel's first; both are live click targets across their full
width (`row_at` is `mx`-independent, `panel_hits` is row-granular). Taking a
column from them to give to the divider swaps one miss for another, and on the
sidebar it would also arm a row-reorder drag and a resize from the same press.
Two cells is the widening that costs nothing; take it.

**The extra cell is conditional.** The non-full-width bottom drawer is laid out
at `x = center_x` (`layout.rs:632-637`) — its content occupies the extra cell in
the drawer's rows. So the extra cell only grabs when `hit_pane.is_none()`, which
is exactly "the pointer is not inside a pane's or the drawer's content rect"
(`handlers/overlay.rs:186-197`). The separator column itself always grabs.

**Degenerate geometry.** With a 1-column center, `sep_left + 1 == sep_right - 1`
(both are the center's only column). The sidebar check runs first (`run.rs:12623`
before `:12636`) and wins. Deterministic; pin it with a test.

**Press arms, motion commits (F2).** Both grabs carry the press column and a
`moved` flag. The press sets no width, does not `collapse_wide`, and shows the
existing hint. The first sample whose pointer column differs from the press
column flips `moved`, and only then does the sidebar drop out of Wide. A release
with `moved == false` restores nothing (nothing changed), persists nothing, and
leaves no "width: N cols" report — a bare click on the divider becomes a no-op
instead of a mutation.

**The divider stays under the cursor (rule 3).** With the offset preserved:

```rust
// crates/thegn-host/src/drag_hit.rs
/// The separator column implied by pointer column `mx` for a grab that pressed
/// at `press_x` while the separator was at `sep` — the grab offset is held for
/// the whole drag, so the divider tracks the cursor instead of jumping to it.
pub fn sep_follow(press_x: usize, sep: usize, mx: usize) -> usize;
```

Sidebar width = `sep_follow(..)` (the separator column _is_ the width, see
`layout.rs:554`); panel width = `cols - sep_follow(..) - 1` (mirroring the
existing `cols.saturating_sub(mx + 1)` at `run.rs:12554`). Clamps stay exactly
where they are.

**Esc cancels (F3).** Extend the existing pane-gesture Esc arm (`run.rs:13886`)
to the two separator drags, restoring the width snapshotted at grab time
(`sb.width` / `panel_cols_pref`), recomputing chrome, and persisting nothing.

### 3.2 Pane surface

- **F5** — `border_at` takes a `slop: usize`; the seam band becomes
  `rf.x - 1 - slop ..= rf.x + slop` (and the row mirror). The call site
  (`run.rs:12654`) passes `PANE_HPAD`. At the default `pane_padding = 0` this is
  byte-for-byte today's behavior; with padding it makes the whole visible gutter
  grabbable. The content early-return (`pane_drag.rs:37-39`) keeps it from
  reaching content at any slop.
- **F4** — `pane_lift` carries `(id, press_x, press_y, moved)`. A release with
  `moved == false` focuses the lifted pane (`focus.zone = Center;
tab.focused_pane = id`, the same two lines as `run.rs:12872-12875`) instead of
  committing nothing. Motion sets `moved` and behaves exactly as today.

### 3.3 Sidebar drag: no dead space inside the surface (F6)

Add a clamping resolver beside `row_at`:

```rust
// crates/thegn-host/src/sidebar_view.rs
/// The rendered row nearest screen row `my`: the row under it, else the first
/// row when `my` is above them all and the last when below. For DRAG samples
/// only — a click on blank space must not select a row.
pub(crate) fn row_at_clamped(hits: &[RowHit], my: usize) -> Option<&RowHit>;
```

`spot_at` (`sidebar_mouse.rs:551`) uses it, but **only for `my` inside the
sidebar rect** (`rect.y .. rect.y + rect.rows`); outside the rect stays
`Spot::Invalid`, so the surface has a boundary and a drag over the panel or
masthead still cancels harmlessly. `on_left_press` / `on_right_press` keep the
strict `row_at` — clicking blank space below the list must not select the last
row.

This is safe by construction: the clamp only chooses a row, and
`spot_for_hover` still validates it (cross-workspace → `Invalid`, home's slot →
`Invalid`, the source row itself → `Invalid`, `sidebar_mouse.rs:581-609`). It
converts "nothing happens" into "the nearest row", never into a wrong drop.

### 3.4 Tab chips (F7)

A tab chip's **hit** span absorbs the single spacing column drawn after it:
`[sx, sx + w + 1)`, clamped so it never crosses the boundary
`strip_chip_spans` already computes (`pin_chips_start` minus the env cluster,
`chrome.rs:844-846`). Painting is untouched — the widening lives in
`center_tab_hit`, which keeps `strip_chip_spans` the one source of placement.
Spans stay non-overlapping (the next chip starts at `sx + w + 1`), so the gap
column resolves to the chip on its left, and columns before the first chip stay
`None` (the regression assertion at `chrome_tests.rs:1429-1433` must keep
passing unchanged).

---

## 4. Invariants this change must not break

- **Render decision stays pure.** Every new path routes through the existing
  `dirty` / `sidebar_dirty` / `bars_dirty` flags. Drag feedback is a chrome
  change → `RenderPlan::Full` (`render_plan.rs:139-141`). **Nothing here may set
  `selection_only`** (`run.rs:11486-11493`) — that fast path is for pane text
  selection and skips chrome recomposition.
- **0% idle.** No timers, no new wake sources, no polling. Every gesture step is
  driven by an inbound mouse event, and the existing `drain_drag_events`
  coalescing (`run.rs:4556-4563`) is untouched.
- **Purity + tests.** All new geometry is a pure function in `drag_hit.rs`,
  `pane_drag.rs`, `chrome.rs` or `sidebar_view.rs`, unit tested at the module.
  No new `thegn-core` code, so the 95% core coverage gate is unaffected.
- **Ratchets.** No color/glyph literals (nothing new is painted), no platform
  `cfg`, no new ignored `Result`s, no `gh`, no new `ACTION_SPECS` action ids —
  so the help ratchet's claim/prose lists do not move. `crates/**` is an
  allowlisted nix source root (`nix/source.nix:28`), so a new module needs no
  packaging change.
- **god-files.** New logic goes in the new `drag_hit.rs` module; `run.rs` gets
  call-site edits only, no new helper bodies.

---

## 5. Chunks

| Chunk | Scope                                                                                                | Files                                                          | Runs                       |
| ----- | ---------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- | -------------------------- |
| **1** | Pure hit geometry: `drag_hit.rs` (band + follow) and the tab-chip gap                                | `drag_hit.rs` (new), `main.rs`, `chrome.rs`, `chrome_tests.rs` | **parallel with 3**        |
| **2** | Loop wiring: separator band/arm/Esc, pane seam slop, pane click-to-focus, help docs, openspec change | `run.rs`, `pane_drag.rs`, `docs/help/*`, `openspec/changes/*`  | **after 1** (uses its API) |
| **3** | Sidebar drop-target clamp                                                                            | `sidebar_view.rs`, `handlers/sidebar_mouse.rs`                 | **parallel with 1**        |

File sets are disjoint. Chunk 2 depends on chunk 1's `drag_hit` API and must
land after it; chunk 3 is independent of both. Chunk 2 lands last and therefore
owns every prose artifact (help pages + the openspec change), so the docs
describe the finished behavior of all three.
