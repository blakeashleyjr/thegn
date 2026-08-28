# Chunk 3 — Sidebar drag: no dead drop zone inside the sidebar

**Issue:** THE-67 (drag/drop needs unnecessary mouse precision).
**Design:** `.thegn/pipeline/THE-67/architect/design.md` — read §2.1 (what is
already correct and must not be redesigned) and §3.3.
**Runs:** in **parallel with chunk 1**. Independent of chunks 1 and 2: no shared
files, no shared API.

## Why

The sidebar row drag is otherwise in good shape — the drop target is the full
visual extent of the row (`row_at`, `sidebar_view.rs:1132`, ignores `mx`
entirely) and the hit table is derived from the same `build_sidebar` pass that
painted (`hit_rows`, `sidebar_view.rs:1096`).

The gap is `spot_at` (`sidebar_mouse.rs:551-557`):

```rust
match row_at(&hits, my) {
    Some(hit) => spot_for_hover(&model.sidebar_rows, hit.visible_index, src),
    None => Spot::Invalid,
}
```

Every sample that lands on no painted row is `Invalid`. Inside the sidebar's own
rect that happens in the **blank tail below the last row** (and above the first
painted row) — regions that look like part of the list and are inside the
surface the drag owns. Releasing there silently does nothing, which reads
exactly as "the drop needs unnecessary precision".

## Files you own

- `crates/thegn-host/src/sidebar_view.rs`
- `crates/thegn-host/src/handlers/sidebar_mouse.rs`

Do not touch `run.rs`, `chrome.rs`, `chrome_tests.rs`, `pane_drag.rs`,
`drag_hit.rs`, `docs/`, or `openspec/` — chunks 1 and 2 own those. In
particular **do not change `row_at`** (`chrome_tests.rs` calls it and chunk 1
owns that file).

## Approach

### 1. `sidebar_view.rs` — a clamping resolver beside `row_at`

Add, immediately after `row_at` (`sidebar_view.rs:1132`):

```rust
/// The rendered row nearest screen row `my`: the row under it when there is
/// one, else the FIRST row when `my` is above them all and the LAST when it is
/// below. For DRAG samples only — a click on blank space must not select a row,
/// so `on_left_press` / `on_right_press` keep the strict [`row_at`].
pub(crate) fn row_at_clamped(hits: &[RowHit], my: usize) -> Option<&RowHit>;
```

`hits` comes from `frame.rows` in paint order, so it is sorted by `y`; resolve
"above all" / "below all" with `first()` / `last()` rather than a min/max scan,
but do not assume non-empty — return `None` for an empty slice.

Unit tests in `sidebar_view.rs`'s existing test module, beside
`row_at_maps_screen_row_into_row_bounds` (`sidebar_view.rs:2026`) and using the
same fixture shape:

- a `my` inside a row resolves identically to `row_at`;
- `my` below the last row returns the last row (where `row_at` returns `None`,
  `sidebar_view.rs:2037`);
- `my` above the first row returns the first row (`sidebar_view.rs:2029`);
- an empty `hits` slice returns `None`.

### 2. `handlers/sidebar_mouse.rs` — use it for drag samples only

`spot_at` currently takes `(model, rect, src, my)` and ignores `rect` except to
build the hits. Change its body to:

- if `my` is **outside** the sidebar rect's rows (below `rect.y`, or at/past
  `rect.y + rect.rows`) → `Spot::Invalid`, unchanged. The surface keeps a
  boundary: a drag that wanders onto the masthead or the panel still resolves to
  nothing and releases harmlessly.
- otherwise → `row_at_clamped`, and `Spot::Invalid` only if it returns `None`
  (no rows painted at all).

Nothing else changes. `on_left_press` (`sidebar_mouse.rs:203`) and
`on_right_press` (`:304`) keep the strict `row_at` — clicking blank space below
the list must not select the last row.

Update `spot_at`'s doc comment to record the split: the drag path clamps to the
nearest row so the sidebar's rect has no dead space; the click path does not.

**Why this is safe, and what to say in the comment:** the clamp only _chooses_ a
row — `spot_for_hover` still validates it and returns `Spot::Invalid` for a
cross-workspace row, for home's anchored slot, and for the source row itself
(`sidebar_mouse.rs:581-609`). So the change converts "nothing happens" into "the
nearest row", never into a wrong drop.

### 3. Do NOT do these

Called out because the issue text invites them and the code has already
rejected them (design §2.1):

- **No between-rows insertion band.** The rule is displacement, and the reason a
  half-row split was removed is written at `sidebar_mouse.rs:559-571`. A 1-cell
  row has no sub-cell to split; an insertion band would halve a target that is
  currently 100% live.
- **No change to the `Pressed → Dragging` threshold** (`sidebar_mouse.rs:389-413`).
  It already re-derives the pressed row's band from the row's live placement
  each sample, which is the "small threshold" the design asks for.
- **No change to `autoscroll_step`** (`sidebar_mouse.rs:445-459`).

### 4. Tests for the new behavior

Add to `sidebar_mouse.rs`'s test module, in the style of
`pressed_becomes_dragging_only_after_leaving_the_row_band`
(`sidebar_mouse.rs:1077`):

- a drag sample in the **blank tail** below the last row, but inside the rect,
  resolves to the same `Spot` as a sample on the last row (previously
  `Spot::Invalid`);
- a sample **outside** the rect (e.g. `my = rect.y + rect.rows`) is still
  `Spot::Invalid`;
- a clamped sample that lands on a row in **another workspace** is still
  `Spot::Invalid` — the clamp does not bypass `spot_for_hover`'s validation.

## Tests to run (scoped — no full-workspace gates)

```sh
just quick thegn-host
cargo nextest run -p thegn-host row_at
cargo nextest run -p thegn-host sidebar_mouse
```

Do **not** run `just test`, `just ci`, `just coverage`, or `just e2e`.

## Done criteria

- `row_at_clamped` exists in `sidebar_view.rs` with its four unit tests passing;
  `row_at` is byte-for-byte unchanged.
- `spot_at` clamps inside the sidebar rect and stays `Invalid` outside it, with
  the three new tests passing.
- The press paths still use strict `row_at`.
- `just quick thegn-host` is clean.

**Commit subject (exact):**

```
fix(the-67): sidebar drag drops land on the nearest row, not nothing
```
