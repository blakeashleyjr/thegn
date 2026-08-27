# THE-64 — chunk 2: header tiers + workspace separator gaps

Read `.thegn/pipeline/THE-64/architect/design.md` in full before starting — §2
(tiers), §3 (the gap mechanism), §4 (why the gap is not a synthetic row) and §5
(the two deliberate deviations) are the spec. Work in
`/home/blake/.superzej/worktrees/thegn/tg-the-64-sidebar-distinction` on branch
`tg/the-64-sidebar-distinction`.

## Ordering

**Runs AFTER chunk 1**, which adds `UiConfig::sidebar_dividers`. This chunk
reads that field in `SidebarDisplay::from_ui` and will not compile until chunk 1
has landed. File sets are disjoint from chunk 1's — the dependency is the field.

## Files touched (exact)

- `crates/thegn-host/src/sidebar_view.rs` — the bulk of the work
- `crates/thegn-host/src/chrome_tests.rs` — geometry/hit tests
- `crates/thegn-host/src/handlers/sidebar_mouse.rs` — one caret-hit guard
- `docs/help/sidebar.md`
- `CHANGELOG.md`
- `openspec/changes/add-sidebar-visual-hierarchy/specs/sidebar/spec.md`

## Part A — the three tiers (`sidebar_view.rs`)

Today `Workspace`, `SectionHeading` and `Folder` labels are all
`seg(Tok::Slot(S::Text), …).bold()` (lines 1413, 1433, 1478) on the same
`S::Bg0` band (`row_bg`, lines 1057-1068). Split them:

1. `compose_row_lines`, `RowKind::Workspace | RowKind::TerminalHost` arm
   (lines 1385-1430):
   - Label at line 1413 → `seg(Tok::Slot(S::Accent), row.label.clone()).bold()`.
   - Lead glyph after the caret: the existing `dir` arm (lines 1409-1412) keeps
     `gl.dir` but moves `S::Text` → `S::Accent`; add an `else` for a plain git
     workspace pushing `seg(Tok::Slot(S::Accent), format!("{} ", gl.diamond_filled))`.
   - The `TerminalHost` arm (lines 1400-1408) keeps `gl.host_local` /
     `gl.host_remote` in `S::Dim` — that glyph carries local-vs-remote meaning,
     not tier — but its label still takes the accent+bold treatment.
2. `compose_row_lines`, `RowKind::Folder` arm (lines 1464-1480):
   - Folder glyph `S::Dim` → `S::Faint` (line 1477).
   - Drop `.bold()` from the label and split the count out: push
     `seg(Tok::Slot(S::Text), row.label.clone())` and then, when
     `row.child_count > 0`,
     `seg(Tok::Slot(S::Faint), format!(" ({})", row.child_count))`. Delete the
     `label` `format!` at lines 1467-1471.
3. `row_bg` (lines 1057-1060): remove `RowKind::Folder` from the `header`
   predicate so it falls through to `Tok::Slot(S::Panel)`. Workspace and
   TerminalHost keep `S::Bg0` and become the only banded tier. Update the
   doc-comment at lines 1034-1036, which currently says
   "workspace/host/folder".

`SectionHeading` (line 1433) is unchanged — it is a title, not a row.

**Do not touch column geometry.** The new glyph goes _after_ the caret, so the
workspace caret stays at `rect.x + 4` and the folder caret at `rect.x + 3`
(`hit_rows`, lines 1110-1118). If a caret column moves, you have broken the
click affordance.

**No literals.** Every color goes through `Tok::Slot(S::…)` and every glyph
through `crate::caps::active_glyphs()`. `S::Accent` and `gl.diamond_filled`
(`◆`, ASCII `*`) both already exist — do **not** add a `GlyphSet` field, and do
**not** add an entry to `test/color-literal-ratchet.txt` or
`test/glyph-literal-ratchet.txt`.

## Part B — the separator gap (`sidebar_view.rs`)

Reuse the existing `SectionHeading` breathing-gap machinery rather than adding a
row kind. Add two private helpers and route both existing call sites through
them:

```rust
/// Blank rows laid out ABOVE visible row `i`, before its own lines. The single
/// source for the height pass, the compose pass and hit-testing — they must
/// agree or `build_sidebar`'s `debug_assert_eq!` fires.
fn lead_gap_rows(model: &FrameModel, visible: &[&crate::sidebar::SidebarRow], i: usize) -> usize {
    use crate::sidebar::RowKind;
    if model.sidebar_rail || i == 0 {
        return 0;
    }
    match visible[i].kind {
        RowKind::SectionHeading => 1,                  // pre-existing breathing gap
        RowKind::Workspace if dividers_on(model) => 1, // THE-64 repo boundary
        _ => 0,
    }
}

fn dividers_on(model: &FrameModel) -> bool {
    model.sidebar_display.dividers
        && !model.sidebar_filtering
        && model.sidebar_filter.is_empty()
}
```

Then:

1. **`SidebarDisplay`** (lines 24-38): add `pub dividers: bool`; `Default` =
   `true` (lines 40-57); `from_ui` reads `ui.sidebar_dividers` (lines 59-76).
2. **Height pass** — `sidebar_geom_from` (lines 508-530): replace the inline
   `if row.kind == RowKind::SectionHeading && i > 0 { h += 1 }` at lines 524-527
   with `h += lead_gap_rows(model, visible, i);`. This is what makes the gap
   count in `max_sidebar_scroll` (line 906) and `sidebar_window`'s
   hidden-above/below tallies (line 1001) — do not compute it anywhere else.
3. **Compose pass** — `build_sidebar` (lines 614-683): replace the inline
   `SectionHeading` insert at lines 651-654 with
   `let lead_gap = lead_gap_rows(model, &visible, i);` and inserting that many
   `crate::seg::Line::Blank`s at the head of `lines`. Keep the insert **before**
   the `debug_assert_eq!` at lines 655-660 (which must see the untrimmed
   vector), and generalize the clipped-tail trim at lines 671-673 from
   `row.kind == RowKind::SectionHeading && i > 0` to `lead_gap > 0` — a
   partly-fitting workspace row must drop its gap, not its label.
4. **`SidebarPlacement`** (lines 360-367): add `pub lead_gap: usize`, populated
   from the above.
5. **Paint** — `draw_sidebar` (lines 231-263): split the single `draw_lines`
   call into two — the first `p.lead_gap` rows with `crate::seg::Tok::Slot(S::Panel)`
   and the remaining `p.height - p.lead_gap` rows (starting at
   `p.y + p.lead_gap`, slicing `&p.lines[p.lead_gap..]`) with `p.bg`.

   **This split is load-bearing, not cosmetic.** `draw_lines` fills the whole
   rect with one `pad_bg` (`seg.rs:577-582`), so a gap painted on the header's
   own `S::Bg0` would just make the band two rows tall — two abutting collapsed
   workspaces would merge into one four-row band, i.e. THE-64 made worse. Verify
   this by eye before you call the chunk done.

   The `cursor_bar` loop (lines 259-261) must start at `p.lead_gap` so the
   cursor bar marks the row, not the blank above it.

6. **Hit-testing** — `RowHit` (lines 1084-1093): add `pub lead_gap: usize`,
   populated from the same placement in `hit_rows` (lines 1096-1129).
   **`row_at` (lines 1132-1134) stays unchanged** — a placement owns its full
   height including the gap. That is deliberate (design §5): a click on the gap
   resolves to the workspace header below it, exactly as the `SectionHeading`
   gap already behaves, which keeps `chrome_tests.rs:1012-1023`'s full-height
   hit-coverage assertion green and makes a drag over the gap resolve to the
   header's run boundary through `spot_at` (`sidebar_mouse.rs:551-557`) with no
   edit.

## Part C — the caret guard (`handlers/sidebar_mouse.rs`)

At `sidebar_mouse.rs:210` the press path toggles collapse whenever
`hit.caret_x == Some(mx)`, regardless of which line of the row was clicked. With
a lead gap that would make the blank line's caret column toggle collapse — a
click target with nothing under it. Gate it on the row's own lines:

```rust
if hit.caret_x == Some(mx) && my >= hit.y + hit.lead_gap && hit.kind.is_collapsible() {
```

This is the only edit in this file.

## Part D — docs

- `docs/help/sidebar.md`: a short paragraph describing the three tiers (repo
  headers accented + bold with `◆`; folders a quieter grouping inside a repo;
  worktrees the body) and the `[ui] sidebar_dividers` opt-out. **No new action
  ids** — the help ratchets (`test/help-ratchet.txt`,
  `test/help-prose-ratchet.txt`, `test/help-context-ratchet.txt`) must not
  change. Do not hand-write a config-reference entry; that page is generated.
- `CHANGELOG.md`: an entry under `## [Unreleased]`, house style (a bolded
  lead sentence then the detail). Say plainly that **e2e baselines have not
  been re-recorded** in this change.
- `openspec/changes/add-sidebar-visual-hierarchy/specs/sidebar/spec.md`: amend
  the "The gap is interaction-transparent" scenario (lines 48-53) to match what
  was built — a click over the gap resolves to the workspace header it precedes
  (the same rule the section-heading breathing gap follows), while a drop over
  it still lands exactly where a drop on the adjacent run boundary would land,
  and the caret cell is inert on the gap line. Leave the other three scenarios
  as written; they are all satisfied. Keep the `### Requirement:` /
  `#### Scenario:` shape intact — `openspec validate --all --strict` runs in
  `just ci`.

## Tests

`sidebar_view.rs`'s `mod tests` (line 1760) and `chrome_tests.rs`:

- **tiers without color**: a `Workspace` row's label seg is bold and an accent
  slot; a `Folder` row's label seg is **not** bold; each carries its distinct
  lead glyph. Assert on the composed `Vec<Line>`/segs, not on resolved colors,
  so the test states the mono-safe part of the contract.
- **`row_bg`**: `Workspace` and `TerminalHost` resolve to `Tok::Slot(S::Bg0)`;
  `Folder` resolves to `Tok::Slot(S::Panel)`; `SectionHeading` unchanged.
- **gap layout**: with two workspaces and dividers on, the second workspace's
  placement has `lead_gap == 1` and `height` one greater than with dividers off.
- **dividers off ⇒ identical layout**: `sidebar_geom(...).heights` with
  `dividers = false` is element-wise equal to the same model's heights before
  the change (assert against the ungapped expectation, e.g. all-ones for a
  simple two-workspace model).
- **suppression**: no gaps in rail mode (`sidebar_rail = true`), none while
  `sidebar_filtering` is set, none while `sidebar_filter` is non-empty.
- **scroll geometry**: `max_sidebar_scroll` and `sidebar_window`'s
  `hidden_above`/`hidden_below` grow by the number of gaps — the truncation
  chips must stay truthful.
- **hit coverage**: extend `chrome_tests.rs:1012-1023`'s existing loop to a
  two-workspace gapped model; every screen line of every placement still
  resolves to its own `visible_index`.
- **caret guard**: a click at `caret_x` on the gap line does not toggle
  collapse; the same column on the label line does.

`chrome_tests.rs:909`'s `many_rows` helper builds a one-workspace tree — add a
sibling helper for a two-workspace tree rather than changing it (existing tests
depend on its shape).

Run — **scoped only**:

```sh
just quick thegn-host
cargo nextest run -p thegn-host sidebar
cargo nextest run -p thegn-host chrome
```

Do **not** run `just test`, `just ci`, `just coverage`, `just e2e`, or
`just e2e-update`.

## Frame change (record, do not act on)

Every muse baseline showing the sidebar moves: `sidebar__focused`,
`chrome_regions__chrome`, `responsive_breakpoints__layout`, `panel_git__branches`,
`panel_system__system`, `panel_work__work`, all four `themes__*`, and the
`glitch_hunt_*` cases. Note this in the commit body and the CHANGELOG.
**Re-recording is out of scope for this chunk** — leave it to a deliberate
later pass.

## Done criteria

- Parts A–D complete; the three scoped commands above pass.
- No `test/*-ratchet.txt` file changed; no `GlyphSet` field added; no color or
  glyph literal at a draw site.
- Workspace caret still at `rect.x + 4`, folder caret at `rect.x + 3`.
- Committed on `tg/the-64-sidebar-distinction` with **exactly** this subject:

  ```text
  feat(sidebar): tier headers and separate workspaces (THE-64)
  ```
