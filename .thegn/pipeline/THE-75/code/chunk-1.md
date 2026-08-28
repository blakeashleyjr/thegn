# THE-75 chunk 1 — Numbered tabs, a real row cursor, and a viewport that follows it

Read `.thegn/pipeline/THE-75/architect/design.md` §1.1–§1.3 and §D1–§D3 first —
the evidence and the rationale are there; this file is the work order.

Covers audit items **5 (M)**, **6 (M)** and **7 (M)**.

## Ordering / overlap

- **Runs FIRST. Serial.** Chunks 2 and 3 both edit `monitor.rs`,
  `monitor/build.rs` and `monitor_tests.rs`. Do not run this in parallel with
  them.
- Chunk 2 depends on this chunk's `TabBuild` return type and on
  `TableSection.sel`; chunk 3 depends on `monitor/footer.rs` existing? **No** —
  chunk 3 creates `monitor/footer.rs` itself. Do not create it here.

## Files touched (exact)

| Path                                      | Why                                                                                                                            |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `crates/thegn-host/src/sections.rs`       | `TableSection.sel`, selection painting in `draw_table`, `table_cols` gutter                                                    |
| `crates/thegn-host/src/monitor.rs`        | `row_y`/`follow` state, `nav`/`Home`/`End`/`wheel`, the `0` digit key, `tab_bar()` delegating to the new module, `mod tabbar;` |
| `crates/thegn-host/src/monitor/tabbar.rs` | **NEW** — pure digit + windowing helpers, with their tests                                                                     |
| `crates/thegn-host/src/monitor/build.rs`  | `TabBuild` return, `row_ys` helper, `sel` handed to `TableSection` instead of tinting one cell                                 |
| `crates/thegn-host/src/monitor_tests.rs`  | New tests (see below)                                                                                                          |

Do **not** touch: `run.rs`, `docs/help/`, `thegn-core`, `monitor_pipeline.rs`,
`monitor_action.rs`. Do not add or edit any keymap action.

## Work

### 1. `TableSection` gains a selected row (`sections.rs`)

```rust
pub struct TableSection {
    pub header: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// Index of the cursor row, when this table is a navigable list. `Some`
    /// also turns on a one-cell selection gutter on EVERY row, so columns stay
    /// aligned whichever row is current.
    pub sel: Option<usize>,
}
```

- Every existing construction site must gain `sel: None`. There are several in
  `monitor/build.rs` and at least one in the panel/detail chrome — let the
  compiler find them; do not add `..Default::default()` (the struct has no
  `Default` today and should not get one just for this).
- In `draw_table` (`sections.rs:557`), for each body row:
  - when `t.sel.is_some()`, emit a leading gutter seg:
    `crate::caps::active_glyphs().half_block_r` in `Tok::Slot(S::Accent)` for
    the selected row, a single space otherwise;
  - when `i == sel`, set `bg = Tok::Slot(S::Panel2)` on **every** seg of the row
    (`Seg::bg`, `seg.rs:153`) and pass `Tok::Slot(S::Panel2)` as `put_line`'s
    pad token, so the tint runs the full row width;
  - the header row keeps the same gutter width (a space) so it stays aligned.
- `table_cols` (`sections.rs:548`) must add 1 when `sel.is_some()`.
- **No glyph or color literal.** The bar comes from `active_glyphs()`, the
  tones from `S::` slots. `sections.rs` is on neither ratchet allowlist and
  both are shrink-only.
- `Section::height` for a `Table` must be **unchanged** — the gutter is
  horizontal. `sections.rs:14-17` explains why that matters.

### 2. `build::tab` returns geometry as well as sections (`monitor/build.rs`)

```rust
pub(super) struct TabBuild {
    pub sections: Vec<Section>,
    /// Stack-relative y of each selectable row, in `sel` order. Empty on a tab
    /// with no row cursor.
    pub row_y: Vec<usize>,
}
```

- `pub(super) fn tab(input: TabInput) -> TabBuild`.
- Add the helper from the design (§D1):

```rust
/// Stack-relative y of the `n` body rows of a table about to be pushed onto
/// `out`. Measured with `sections::stack_height` — the same function
/// `scroll_max` measures against — so the cursor and the scroll clamp can
/// never disagree about where a row is.
fn row_ys(out: &[Section], n: usize, has_header: bool) -> Vec<usize> {
    let base = crate::sections::stack_height(out) + has_header as usize;
    (base..base + n).collect()
}
```

- The four list builders (`disk` worktree lane, `procs`, `containers`,
  `pipeline`) call `row_ys(&out, n, true)` immediately **before** pushing their
  table, pass `sel: Some(sel)` on that `TableSection`, and **stop tinting the
  name cell**:
  - `disk` (`build.rs:583-587`): drop the `if i == sel` fork; the name is
    always `Tok::Slot(S::Text)`.
  - `procs` (`build.rs:831-836`): drop the fork; the name is always
    `owner_tone(p.owner)`.
  - `containers` (`build.rs:916-923`): drop the `cur` arm entirely — the
    ownership tint (`Hue::Green` / `S::Ghost`) is restored as the sole
    foreground rule. This is the regression the audit called out.
  - `pipeline` (`build.rs:1034-1039`): drop the fork; the agent name is always
    `Tok::Slot(S::Text)`. Pipeline emits **one table per stage group** — each
    group's `row_ys` must be computed against the stack as it stands at that
    moment, and the group's `sel` is `sel.checked_sub(ix).filter(|&r| r < group.len())`
    (i.e. `None` unless the cursor is inside that group). Concatenate the
    per-group `row_y` runs in group order so the result indexes `sel` globally.
- Non-list tabs return `row_y: Vec::new()`.

### 3. Viewport follows the cursor (`monitor.rs`)

New overlay state beside `sel`:

```rust
/// Stack-relative y of each list row, from the builder — the single source
/// `follow_row` measures against.
row_y: Vec<usize>,
/// Whether the viewport should chase the cursor. Cleared by an explicit
/// wheel scroll (the user took the viewport by hand), re-armed by any
/// cursor key.
follow: bool,   // default true
```

- `rebuild()`: take `let b = build::tab(...); self.body = b.sections; self.row_y = b.row_y;`
  then, after the existing `self.clamp()`, `if self.follow { self.follow_row(); }`.
- Add:

```rust
/// Scroll the minimum distance that brings `sel` into the viewport. A no-op
/// on a tab with no row cursor (`row_y` is empty there).
fn follow_row(&mut self) {
    let Some(&y) = self.row_y.get(self.sel) else { return };
    let (max, rows) = (self.scroll_max(), self.body_rows);
    let s = &mut self.scroll[self.tab.index()];
    if y < *s {
        *s = y;
    } else if rows > 0 && y >= *s + rows {
        *s = y + 1 - rows;
    }
    *s = (*s).min(max);
}
```

(Read `scroll_max()` / `body_rows` into locals **before** taking `&mut
  self.scroll[..]` — the borrow checker will otherwise reject it.)

- `nav()` (`monitor.rs:1027`): on a list tab with rows, move `sel`, set
  `self.follow = true`, and **return without calling `scroll_by`** — the
  viewport is placed by `follow_row` on the rebuild. On a non-list tab (or an
  empty list) keep the existing `scroll_by(delta)`.
- `Home` (`monitor.rs:988-992`): on a list tab set `sel = 0` and
  `follow = true` (drop the direct `scroll[..] = 0` there); on a non-list tab
  unchanged.
- `End` / `G` (`monitor.rs:993-996`): on a list tab set
  `sel = row_len().saturating_sub(1)` and `follow = true`; on a non-list tab
  unchanged.
- `wheel()` (`monitor.rs:673-675`): set `self.follow = false` before
  `scroll_by`. Its scroll behaviour is otherwise **unchanged** — outside-click
  and wheel handling are explicitly out of scope.
- Tab switches (`switch`, the digit arm, `goto_tab`) already reset `sel = 0`;
  also set `follow = true` there.
- Update the `nav` doc comment: it now explains that on a list tab the cursor
  is the navigation and the viewport follows, and **why** — `x` on
  Processes/Disk acts on `sel`, so a viewport that scrolls independently
  retargets a destructive key while the user is looking elsewhere.

### 4. Numbered tabs, `0`, and an unclipped active tab

New module `crates/thegn-host/src/monitor/tabbar.rs` (declared `mod tabbar;` in
`monitor.rs` beside `mod build;`). Keep it pure — no `self`, no caps read inside
the windowing math:

```rust
/// The digit key that jumps to visible tab `i`: `1`–`9`, then `0` for the
/// tenth. `None` past ten (no key can reach it, and the bar says so by
/// omitting the digit rather than by lying).
pub(super) fn digit(i: usize) -> Option<char>

pub(super) struct TabWindow {
    pub start: usize,
    pub end: usize,       // exclusive
    pub clipped_left: bool,
    pub clipped_right: bool,
}

/// The contiguous run of tabs that fits in `width`, always containing
/// `active` WHOLE. `widths[i]` is the drawn width of tab `i` including its
/// leading separator. Grows outward from `active` — right first, then left —
/// so the common case (a bar that fits) returns the whole range unchanged.
pub(super) fn window(widths: &[usize], active: usize, width: usize) -> TabWindow
```

`window` must be total: `active` alone wider than `width` yields
`start..start+1` (the caller lets `draw_line` clip it — documented, not
silently wrong); `widths` empty yields an empty window.

In `monitor.rs::tab_bar` (`:1320`):

- render each tab as `digit(i)` in `Tok::Slot(S::Ghost)` + a space + the label
  (`S::Accent` bold when active, `S::Dim` otherwise); omit the digit + space
  when `digit(i)` is `None`.
- compute the right-hand run (pause marker / coverage note) first, subtract its
  width **plus one** — that is exactly what `draw_line`'s `Line::Split` arm
  leaves the left run (`seg.rs:541-548`) — and hand the remainder to `window`.
- prefix `caps::glyph(Glyph::QuoteOpen)` when `clipped_left`, suffix
  `caps::glyph(Glyph::QuoteClose)` when `clipped_right`, both in
  `Tok::Slot(S::Ghost)`. Reserve their cells in the width you pass to `window`.
  **No `‹`/`›` literal** — use the tokens.

In `handle_key` (`monitor.rs:923-932`), extend the digit arm to accept `'0'` as
index 9. Keep the existing "out of range is a no-op" behaviour and the
`PrefsChanged` outcome (the tab is a persisted preference). One arm, not two —
map the char to an index with `tabbar::digit`'s inverse rather than duplicating
the table.

## Tests

Add to `crates/thegn-host/src/monitor_tests.rs` (the fixtures you need —
`open()`, `open_on`, `full_snap`, `ctx_at` — are already there at
`monitor_tests.rs:19-120`):

1. `the_viewport_follows_the_row_cursor_down_a_long_list` — on Processes with
   more rows than `body_rows`, press `j` past the fold and assert the selected
   row's `row_y` is inside `[scroll, scroll + body_rows)`.
2. `scrolling_never_retargets_the_destructive_key` — the safety regression.
   Drive `j` on Disk/Processes and assert the row `x` would act on
   (`disk_rows[sel]` / `proc_rows[sel]`) is the row that is on screen; assert
   the confirmation label names it.
3. `home_and_end_move_the_cursor_on_a_list_tab` — `End` puts `sel` at the last
   row and scrolls it into view; `Home` returns to row 0.
4. `a_wheel_scroll_stops_the_viewport_chasing_until_the_next_key` — `wheel()`
   then a refresh leaves the viewport put; a subsequent `j` re-arms following.
5. `the_selected_row_is_the_only_one_with_a_selection_background` — inspect the
   built `Section::Table` for the list tabs and assert exactly one `sel`.
6. `selecting_a_container_row_keeps_its_ownership_tint` — the Containers
   regression: with `sel` on an owned row, its name cell is still
   `Tok::Hue(Hue::Green)`, and on a foreign row still `Tok::Slot(S::Ghost)`.
7. In `monitor/tabbar.rs`: `digit` covers `0..=10` including the `'0'` tenth
   and the `None` past it; `window` covers (a) everything fits → whole range,
   no flags, (b) active at the far right → window is right-anchored,
   `clipped_left`, (c) active at the far left → `clipped_right`, (d) `active`
   alone wider than `width` → a one-tab window, (e) empty input.
8. `the_tenth_tab_is_reachable_by_zero` — on a fixture showing all ten tabs,
   `'0'` selects `MonitorTab::Pipeline` and returns `PrefsChanged`.
9. `the_active_tab_is_never_clipped_out_of_the_bar` — open on a narrow screen
   with all ten tabs, walk to the last, and assert the drawn tab-bar `Line`
   contains the active label in full.

Run, scoped only:

```sh
just quick thegn-host
cargo nextest run -p thegn-host monitor
cargo nextest run -p thegn-host sections
```

Do **not** run `just test`, `just ci`, `just coverage`, or `just e2e`.

## Done criteria

- All nine test groups above pass; the existing `monitor_tests.rs` and
  `sections` tests still pass unmodified except where the `TableSection.sel`
  field forced a construction-site edit.
- `just quick thegn-host` is clean (clippy `-D warnings`).
- No new entry in `test/glyph-literal-ratchet.txt` or
  `test/color-literal-ratchet.txt`; no new `let _ =` without a
  `// best-effort:` reason.
- `monitor.rs` did not grow a new subsystem — `tabbar.rs` is its own file.
- Commit with **exactly** this subject:

```
feat(monitor): numbered tabs, a real row cursor, and a viewport that follows it (THE-75)
```
