# THE-75 chunk 1 — done

Branch `tg/the-75-monitor-fixes`. Covers audit items **5 (M)**, **6 (M)**, **7 (M)**.

Commits (oldest first):

| sha        | subject                                                                                    |
| ---------- | ------------------------------------------------------------------------------------------ |
| `4a7400eb` | `wip(monitor): TableSection.sel, TabBuild row geometry, tabbar windowing (THE-75)`         |
| `2355bf9f` | `feat(monitor): numbered tabs, a real row cursor, and a viewport that follows it (THE-75)` |

The final commit subject is the exact string the chunk spec required.

## What landed

### 1. `TableSection.sel` — selection is a table property, painted once

- `crates/thegn-host/src/sections.rs`
  - `TableSection` gains `sel: Option<usize>`.
  - `draw_table` paints the cursor row: a `half_block_r` gutter seg in
    `Tok::Slot(S::Accent)` for the selected row (a single space on every other
    row of a cursor-bearing table, so columns never shift), every seg of the
    selected row gets `.bg(Tok::Slot(S::Panel2))`, and `put_line`'s pad token is
    `S::Panel2` on that row so the tint runs the full width. The header row
    reserves the same one-cell gutter.
  - `table_cols` adds 1 when `sel.is_some()`.
  - `Section::height` for a `Table` is **unchanged** — the gutter is horizontal.
  - No glyph or color literal: the bar is `caps::active_glyphs().half_block_r`,
    the tones are `S::` slots.

Every other construction site gained `sel: None` (no `Default` added, as
specified): `detail.rs` ×2, `detail_tests.rs`, `detail/calendar/render.rs` ×3,
`detail/usage_dash.rs` ×2, `detail/ci_drill.rs` ×2, `detail/status_modal.rs` ×2,
plus the four non-list tables in `monitor/build.rs` (core grid, thermal sensors,
network interfaces, disk volumes).

### 2. `build::tab` returns geometry (`monitor/build.rs`)

- New `pub(super) struct TabBuild { sections, row_y }`, `fn plain(...)` for the
  six non-list tabs, and the `row_ys(out, n, has_header)` helper from §D1 —
  measured with `sections::stack_height`, the same function `scroll_max` uses.
- The four list builders (`disk` worktree lane, `procs`, `containers`,
  `pipeline`) call `row_ys` immediately before pushing their table, pass
  `sel: Some(sel)`, and **stopped tinting the name cell**:
  - disk → name is always `S::Text`;
  - procs → name is always `owner_tone(p.owner)`;
  - containers → the `cur` arm is gone entirely; `Hue::Green` / `S::Ghost` is
    restored as the sole foreground rule (the regression the audit called out);
  - pipeline → agent name is always `S::Text`; per stage group the `sel` is
    `sel.checked_sub(ix).filter(|&r| r < group.len())` and each group's `row_y`
    run is computed against the stack as it stands at that moment, concatenated
    in group order so the result indexes `sel` globally.

### 3. Viewport follows the cursor (`monitor.rs`)

- New state: `row_y: Vec<usize>` and `follow: bool` (default `true`).
- `rebuild()` takes `b.sections` / `b.row_y`, then after the existing `clamp()`
  runs `follow_row()` when following.
- `follow_row()` scrolls the minimum distance that brings `sel` into view; a
  no-op where `row_y` is empty. Bounds are read into locals before the `&mut`
  borrow of the scroll slot, as the spec noted.
- `nav()` on a list tab **with rows** moves `sel`, sets `follow = true` and
  returns without `scroll_by`; a non-list tab (or an empty list) keeps the old
  `scroll_by(delta)`. Its doc comment now explains the `x`-retarget safety
  rationale. Extracted `is_list_tab()` so `nav`/`Home`/`End` share one predicate.
- `Home`: `sel = 0`, `follow = true`; the direct `scroll[..] = 0` now only runs
  on a non-list tab. `End`/`G`: on a list tab `sel = row_len() - 1`,
  `follow = true`; otherwise unchanged.
- `wheel()` clears `follow` before `scroll_by` — scroll behaviour otherwise
  untouched.
- `switch`, `goto_tab` and the digit arm also set `follow = true`.

### 4. Numbered tabs, `0`, and an unclipped active tab

- New `crates/thegn-host/src/monitor/tabbar.rs` (declared `mod tabbar;`), pure:
  `digit(i)`, `index_of(c)` (the inverse, implemented by asking `digit` so no
  second table can drift), `TabWindow`, and
  `window(widths, active, width)` growing right-then-left from `active`. Total:
  an oversized active tab yields `start..start+1`, empty input yields an empty
  window. Four unit tests in place.
- `tab_bar()` now renders `digit(i)` in `S::Ghost` + a space + the label
  (`S::Accent` bold when active, `S::Dim` otherwise), omitting the prefix when
  `digit(i)` is `None`. It computes the right run first, subtracts
  `right_w + 1` (exactly what `draw_line`'s `Line::Split` arm leaves the left
  run), and hands the remainder to `window`. Overflow markers are
  `caps::glyph(Glyph::QuoteOpen)` / `QuoteClose` in `S::Ghost` — no literal.
  Per-tab segs are built first and measured with `seg_width`, so the widths the
  window is computed from are exactly the widths drawn.
- `handle_key`'s digit arm is now a single `KeyCode::Char(c @ '0'..='9')` arm
  routed through `tabbar::index_of`; out-of-range stays a no-op and the outcome
  is still `PrefsChanged`.

## Tests

All nine groups from the spec, plus the tabbar unit tests:

| #   | test                                                                                                                                                                                                                             |
| --- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `the_viewport_follows_the_row_cursor_down_a_long_list`                                                                                                                                                                           |
| 2   | `scrolling_never_retargets_the_destructive_key` (Processes **and** Disk)                                                                                                                                                         |
| 3   | `home_and_end_move_the_cursor_on_a_list_tab`                                                                                                                                                                                     |
| 4   | `a_wheel_scroll_stops_the_viewport_chasing_until_the_next_key`                                                                                                                                                                   |
| 5   | `the_selected_row_is_the_only_one_with_a_selection_background` (incl. a two-stage board fixture, so the "only the cursor's group carries it" claim is real)                                                                      |
| 6   | `selecting_a_container_row_keeps_its_ownership_tint`                                                                                                                                                                             |
| 7   | `monitor::tabbar::tests::{digits_cover_ten_tabs_and_stop, a_bar_that_fits_is_shown_whole, the_window_anchors_to_whichever_end_the_cursor_is_on, an_oversized_active_tab_still_yields_a_window, an_empty_bar_is_an_empty_window}` |
| 8   | `the_tenth_tab_is_reachable_by_zero`                                                                                                                                                                                             |
| 9   | `the_active_tab_is_never_clipped_out_of_the_bar`                                                                                                                                                                                 |

Test helpers added to `monitor_tests.rs`: `SHORT` (an 80×18 box so any list
overflows), `press`, `open_tab`, `model_with_n_procs`, `model_with_n_worktrees`,
`model_with_two_stages`, `cursor_on_screen`, `cursor_table`, `cursor_tables`,
`cell_tone`, `line_text`.

### What was run (scoped only, per the dev-loop policy)

```
just quick thegn-host                                 clean (clippy -D warnings)
cargo nextest run -p thegn-host monitor --no-fail-fast 87/87 pass
cargo nextest run -p thegn-host sections               70/70 pass
cargo nextest run -p thegn-host detail                 88/88 pass
cargo nextest run -p thegn-host ratchet                12/12 pass
```

The ratchet run covers `glyph_literals_go_through_active_glyphs`,
`color_literals_stay_in_the_chokepoints`, the caret ratchets and the help
ratchets — all green with **no new allowlist entries** in
`test/glyph-literal-ratchet.txt` or `test/color-literal-ratchet.txt`.

No new `let _ =` / `.ok()` was introduced. `monitor.rs` did not grow a new
subsystem: `tabbar.rs` is its own file.

## Deviations from the spec (deliberate, small)

1. **Home/End resting position.** The spec drops the direct `scroll[..] = 0`
   from `Home` on a list tab, so `follow_row`'s _minimum-distance_ rule leaves
   the viewport at `row_y[0]` (row 0 flush with the top edge) rather than at
   `scroll == 0` (which would also show the tab heading and the table header).
   Test 3 asserts `scroll == row_y[0]` accordingly. If the intent was for `Home`
   to reveal the heading too, that is a one-line change in the `Home` arm — call
   it out at review.
2. **Overflow-marker width reservation.** Which side is clipped isn't known
   until `window` has run, so `tab_bar` reserves **both** marker cells whenever
   `widths.sum() > avail`, rather than iterating to a fixed point. It
   over-reserves by at most one cell when only one side clips, and never
   under-reserves (which would let the marker push the active tab off the end).
   Documented at the call site.
3. **`is_list_tab()`** was extracted rather than repeating the four-variant
   `matches!` in `nav`, `Home` and `End`.

## Unverified

- **No full-workspace gate was run** (`just test`, `just ci`, `just coverage`,
  `just lint`) — the chunk spec and the Lead addenda forbid it. In particular
  `thegn-core` coverage, cross/feature/MSRV checks, `test-doc` and `nix-build`
  are unverified for this chunk. Nothing here touches `thegn-core`, so the
  95%-line core gate should be unaffected.
- **`just e2e` was not run**, per the design's §4 note: every chunk in this lane
  changes drawn frames (the tab bar now carries digits and overflow markers, the
  list tables gained a selection gutter), so all 45 baselines under
  `test/muse/snapshots/` are stale after this lane. Re-recording with
  `just e2e-update` is a follow-up for whoever revives that gate.
- **Tests outside the `monitor` / `sections` / `detail` / `ratchet` filters were
  not run.** The `TableSection.sel` field forced edits at ten construction sites
  across `detail*`, and the `detail` filter covered those, but a whole-crate
  `cargo nextest run -p thegn-host` has not been done — that is the Lead's
  pre-push gate.
- **`table_cols` callers were not audited by hand.** The function now returns one
  more column for a cursor-bearing table; the only such tables are the four
  monitor list tables, and the monitor sizes its box from `Self::dims`, not from
  `table_cols`. The `table_cols` callers (weather/calendar sizing in `detail`)
  all pass `sel: None` tables, so they are numerically unchanged — but this was
  reasoned from the call sites rather than measured.
- The **`⏸` literal in `tab_bar`'s paused right-run is pre-existing** and was
  left alone; it is core-adjacent glyph-ladder debt of the same family as §1.6
  (chunk 2's `AgentDispatchStatus::glyph_token` work), not chunk 1's scope.
