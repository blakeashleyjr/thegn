# THE-64 — architect design: sidebar visual distinction

Linear: <https://linear.app/blakeashley/issue/THE-64> — "More visual distinction
on the left bar, all the folders run together".

Branch `tg/the-64-sidebar-distinction`. Render-only change, no new I/O, no new
capability row, one new documented `[ui]` boolean.

---

## 1. What is actually wrong (evidence)

Every structural row in the full sidebar is styled identically today. Reading
`compose_row_lines` (`crates/thegn-host/src/sidebar_view.rs:1361`):

| kind                         | line                   | label styling                           |
| ---------------------------- | ---------------------- | --------------------------------------- |
| `Workspace` / `TerminalHost` | `sidebar_view.rs:1413` | `seg(Tok::Slot(S::Text), label).bold()` |
| `SectionHeading`             | `sidebar_view.rs:1433` | `seg(Tok::Slot(S::Text), label).bold()` |
| `Folder`                     | `sidebar_view.rs:1478` | `seg(Tok::Slot(S::Text), label).bold()` |

Three different tiers of the tree, one identical treatment: **bold `S::Text`**.

`row_bg` (`sidebar_view.rs:1037`) then paints the same recessed band under all
of them — `sidebar_view.rs:1057-1060` lumps `Workspace | TerminalHost | Folder`
into one `header` predicate that resolves to `Tok::Slot(S::Bg0)`
(`sidebar_view.rs:1068`).

So a workspace header and a folder header differ **only** by:

- 2 cells of indent (`sp(1)+sp(3)` at `sidebar_view.rs:1386-1397` vs
  `sp(1)+sp(2)` at `sidebar_view.rs:1472-1473`), and
- a dim `▪` folder marker (`sidebar_view.rs:1477`).

Neither survives a glance, and neither says "this line is a repo, that line is
a drawer inside it". Worse, **nothing separates one workspace from the next**:
the only vertical gap in the whole tree is the one-row breathing space above a
`SectionHeading` (`sidebar_view.rs:525-527` for the height,
`sidebar_view.rs:652-654` for the blank line). With the merge queue creating
"Merging" / "Merged" / "Needs attention" folders per repo by default, several
open repos stack into one undifferentiated column of bold bands — THE-64.

There is already an accepted openspec proposal for this
(`openspec/changes/add-sidebar-visual-hierarchy/`, written from the Linear
sweep). This design **implements** it, with two deliberate deviations recorded
in §5.

## 2. The three tiers

All styling resolves through the existing chokepoints — `seg::Tok::Slot(S::…)`
→ `wire.rs::color_spec`, and `crate::caps::active_glyphs()` for glyphs. **No
new theme slot and no new glyph-table entry**: `S::Accent` already exists
(`chrome.rs:70`, `chrome.rs:123`) and `diamond_filled` already exists in both
glyph sets (`termcaps.rs:360` declaration, `termcaps.rs:433` `◆` U+25C6,
`termcaps.rs:497` ASCII `*`) described in-tree as the "generic emphasis
marker" — which is exactly the job. Neither `sidebar_view.rs` nor
`handlers/sidebar_mouse.rs` appears in `test/color-literal-ratchet.txt` or
`test/glyph-literal-ratchet.txt`, so the change must not introduce a literal;
using existing slots/glyphs keeps both allowlists untouched.

| tier            | row kinds                   | glyph                                                             | weight   | fg                                            | band (`row_bg`)                 |
| --------------- | --------------------------- | ----------------------------------------------------------------- | -------- | --------------------------------------------- | ------------------------------- |
| **1 — project** | `Workspace`, `TerminalHost` | `◆` (or the existing `⌂` for `dir` workspaces, `≡`/`⇅` for hosts) | **bold** | `S::Accent`                                   | `S::Bg0` (the only banded tier) |
| **2 — group**   | `Folder`                    | `▪` (unchanged, dropped to `S::Faint`)                            | plain    | `S::Text` + `S::Faint` count                  | `S::Panel` (band removed)       |
| **3 — body**    | `Worktree`, `Terminal`      | tree connector                                                    | plain    | `S::Dim` / `S::Text` / `S::Focus` (unchanged) | `S::Panel` (unchanged)          |

Read as a ladder: tier 1 is the only row with a band, the only row in the
accent, and the only row with `◆`. Tier 2 keeps bold's absence as its main
signal against tier 1 and keeps `▪` + indent against tier 3.

**This is deliberately not color-alone.** Under `theme.color = "mono"` /
16-color quantization the `Bg0`/`Panel` band difference and the `Accent`/`Text`
difference can both collapse, so the ladder is carried redundantly by **weight**
(bold vs plain), **glyph** (`◆` vs `▪` vs `├`/`└`), **indent**, and the
separator gap of §3 — all of which quantize losslessly. `just term-check`'s six
environments therefore cannot break the hierarchy.

Detail per kind (deltas against `compose_row_lines`):

- `RowKind::Workspace | RowKind::TerminalHost` (`sidebar_view.rs:1385-1430`):
  after the caret, push `seg(Tok::Slot(S::Accent), format!("{} ", gl.diamond_filled))`
  for a plain workspace; the existing `dir` arm (`sidebar_view.rs:1409-1412`)
  keeps `gl.dir` but moves from `S::Text` to `S::Accent`; the `TerminalHost`
  arm (`sidebar_view.rs:1400-1408`) keeps `gl.host_local`/`gl.host_remote` in
  `S::Dim` because that glyph carries local-vs-remote meaning, not tier. The
  label seg at `sidebar_view.rs:1413` becomes
  `seg(Tok::Slot(S::Accent), row.label.clone()).bold()`.
- `RowKind::Folder` (`sidebar_view.rs:1464-1480`): folder glyph `S::Dim` →
  `S::Faint`; the label stops being a single formatted string and splits into
  `seg(Tok::Slot(S::Text), row.label.clone())` (**no `.bold()`**) plus, when
  `row.child_count > 0`, `seg(Tok::Slot(S::Faint), format!(" ({})", row.child_count))`.
  The `label`/count split is required so the count can drop a tier without a
  second `format!` allocation of the whole string.
- `row_bg` (`sidebar_view.rs:1057-1060`): drop `RowKind::Folder` from the
  `header` predicate so it falls through to `Tok::Slot(S::Panel)`.

**Column geometry is unchanged.** The workspace caret stays at `rect.x + 4` and
the folder caret at `rect.x + 3` — the new glyph is pushed _after_ the caret, so
`hit_rows`' `caret_x` (`sidebar_view.rs:1110-1118`) needs no edit and the
caret-click affordance stays aligned with what is painted.

## 3. Separating adjacent workspaces

A one-row blank gap is laid out **above** every `Workspace` row that is not the
first laid-out row, gated by a new `[ui] sidebar_dividers` (default `true`).

**Mechanism: reuse the `SectionHeading` breathing-gap precedent verbatim.** The
tree already has exactly this feature, and it is already correct across all
three consumers. Rather than invent a synthetic separator row, generalize the
existing rule into one shared helper:

```rust
/// Blank rows laid out ABOVE visible row `i`, before its own lines. The single
/// source for the height pass, the compose pass and hit-testing.
fn lead_gap_rows(model: &FrameModel, visible: &[&SidebarRow], i: usize) -> usize {
    if model.sidebar_rail || i == 0 { return 0; }
    match visible[i].kind {
        // Pre-existing: a section banner gets breathing space.
        RowKind::SectionHeading => 1,
        // THE-64: repo boundaries are the strongest break in the tree.
        RowKind::Workspace if dividers_on(model) => 1,
        _ => 0,
    }
}

fn dividers_on(model: &FrameModel) -> bool {
    model.sidebar_display.dividers
        && !model.sidebar_filtering
        && model.sidebar_filter.is_empty()
}
```

Wiring, all in `sidebar_view.rs`:

1. **Height pass** — `sidebar_geom_from`, replacing the inline `SectionHeading`
   arm at `sidebar_view.rs:524-527`: `h += lead_gap_rows(model, visible, i)`.
   This is what makes the gap count in `max_sidebar_scroll`
   (`sidebar_view.rs:906`), in `sidebar_window`'s hidden-above/below tallies
   (`sidebar_view.rs:1001`) and therefore in the truncation chips — the
   "truncation is never silent" contract holds for free.
2. **Compose pass** — `build_sidebar`, replacing the inline arm at
   `sidebar_view.rs:651-654`: insert that many `Line::Blank`s at the head, and
   record `lead_gap` on the placement. The clipped-tail trim at
   `sidebar_view.rs:671-673` generalizes the same way (`if height < lines.len()
&& lead_gap > 0 { lines.remove(0) }`) so a partly-fitting workspace row drops
   its gap, not its label — the identical bug the `SectionHeading` trim already
   fixes. The `debug_assert_eq!` lockstep at `sidebar_view.rs:655-660` keeps the
   two passes honest by construction, because both call the same helper.
3. **Paint** — `SidebarPlacement` gains `lead_gap: usize`
   (`sidebar_view.rs:360-367`). `draw_sidebar` (`sidebar_view.rs:236-246`)
   splits its single `draw_lines` into a gap call with
   `Tok::Slot(S::Panel)` and a body call with `p.bg`.

   **This split is required, not cosmetic.** `draw_lines` fills the whole
   placement rect with one `pad_bg` (`seg.rs:577-582`), so a gap left on the
   header's own background would paint in `S::Bg0` and simply make the band two
   rows tall — two abutting collapsed workspaces would merge into one four-row
   band, i.e. the exact bug THE-64 reports, louder. The gap must paint in the
   list background to read as a break. The `cursor_bar` loop
   (`sidebar_view.rs:259-261`) likewise starts at `p.lead_gap` so the cursor bar
   marks the row, not the space above it.

4. **Hit-testing** — `RowHit` (`sidebar_view.rs:1084-1093`) gains `lead_gap:
usize`, populated from the same placement. `row_at`
   (`sidebar_view.rs:1132-1134`) is **unchanged**: a placement owns its full
   height including the gap. `caret_x` is gated on
   `my >= hit.y + hit.lead_gap` at the press site
   (`handlers/sidebar_mouse.rs:210`).

## 4. Why the gap belongs to the header row, not to a synthetic row

The openspec proposal's alternative — a standalone separator entry in the row
vector — was rejected here on blast radius. A new `RowKind` variant is matched
in `sidebar.rs` (build, filter reveal at `sidebar.rs:1569-1658`, `pin_key` at
`sidebar.rs:337`, `is_collapsible` at `sidebar.rs:45`), in `sidebar_view.rs`
(compose, rail compose, `row_bg`, `caret_x`), in `sidebar_order.rs`, in
`sidebar_keytable.rs`, and across `handlers/sidebar_*.rs` — every one of which
must then be taught to skip it. That is precisely the identity-anchor /
hit-target trap family the sidebar audit burned down, re-opened for a blank
line.

Attaching the gap to the following header instead:

- **needs no new row kind at all** — cursor movement, `j/k`, quick-jump,
  re-anchor, filter, reorder runs and `sidebar_order::block_end` are untouched,
  because the visible-row vector does not change;
- **keeps hit-testing and paint derived from one pass**, which is the
  standing sidebar contract (`sidebar_view.rs:227-229`, `sidebar_view.rs:375-378`);
- **preserves the existing hit-coverage invariant.**
  `chrome_tests.rs:1012-1023` asserts every screen line of every placement
  resolves back to that placement's `visible_index`. A synthetic row that is
  drawn but absent from the hit table breaks that test; a gap inside the
  header's placement keeps it green with no edit.

The cost is one behavioural difference from the openspec delta, recorded next.

## 5. Deviations from `add-sidebar-visual-hierarchy` (deliberate)

1. **A click on the gap resolves to the workspace header below it**, not to
   empty space. The delta spec's "click over it resolves as empty space" clause
   assumed a standalone row. With the gap inside the header placement, the
   header simply has a 2-row hit box — the same rule the `SectionHeading`
   breathing gap has followed since it shipped, and a Fitts's-law improvement
   rather than a regression. The delta's **drag** clause is satisfied exactly as
   written: `spot_at` (`handlers/sidebar_mouse.rs:551-557`) resolves the hover
   to the header's `visible_index`, so a drop over the gap lands where a drop on
   the adjacent run boundary lands (`spot_for_hover`'s `RowKind::Workspace` arms
   at `sidebar_mouse.rs:594` and `sidebar_mouse.rs:683-712`). The caret cell is
   the one exception, gated in §3.4, because toggling collapse from a blank line
   would be a click target with nothing under it.
   **The delta spec must be edited to match** — see chunk 2's tasks.
2. **No gap before the TERMINALS section.** It already has one
   (`sidebar_view.rs:525`); adding a second would double it. The shared helper
   makes this structurally impossible rather than a matter of care.

Everything else in the proposal holds: default on, off ⇒ byte-identical
layout, suppressed in rail and under `/`.

## 6. Config

`[ui] sidebar_dividers: bool` (default `true`) in `thegn-core`
(`crates/thegn-core/src/config_ui.rs`, field beside `sidebar_nav_skips_collapsed`
at `config_ui.rs:71`, default at `config_ui.rs:125`), documented in
`config/config.toml.example` beside the other `sidebar_*` keys
(`config/config.toml.example:130-135`).

A `bool` adds **no** `config_enum` definition, so the pinned count of 90 at
`crates/thegn-core/src/config_validate.rs:619-624` is untouched. What does gate
it is `crates/thegn-core/tests/config_example.rs` — a drift test that fails
unless every `Config` key is documented in the example file. The help
config-reference page is generated from that same example
(`crates/thegn-core/src/help/config_ref.rs:284`), so no help page is hand-written
and the help ratchets (`test/help-ratchet.txt`,
`test/help-prose-ratchet.txt`, `test/help-context-ratchet.txt`) are unaffected —
this change adds no action id, keybind, zone or panel section.

It reaches the view through `SidebarDisplay` (`sidebar_view.rs:24-38`,
`from_ui` at `sidebar_view.rs:59-76`), the established pattern that keeps the
pure composers config-free. `Default` is `true`, matching the config default, so
every unit-built `FrameModel` renders dividers.

## 7. Performance and invariants

- **Render decision unchanged.** This is chrome composition; a sidebar change
  is a `Full` frame exactly as today. `render_plan::plan` and its exhaustive
  tests are untouched — no new wake source, no new producer, no poll.
- **No new per-frame allocation of consequence.** The gap adds one
  `Line::Blank` (a unit variant) per workspace boundary to a vector that is
  already built per laid-out row, and only for rows inside the window
  (`build_sidebar` composes only the visible slice —
  `sidebar_view.rs:610-614`). The folder-label split replaces one `format!` of
  `"{label} ({n})"` with one `clone()` plus a small `format!` of the count.
- **`thegn-core` stays substrate-free**; its share of this change is one `bool`
  field, and its 95% line-coverage gate is satisfied by the config round-trip
  test in chunk 1.
- **Ratchets:** no color literal, no glyph literal, no `#[cfg]`, no ignored
  `Result`, no `async fn` in a trait — every allowlist in `test/*-ratchet.txt`
  is unchanged (never grown).

## 8. Testing

Unit tests are the gate; **e2e is not run in this change** (per the Lead, and
per `CLAUDE.md`'s standing note that the suite's baselines are stale).

Row layout, in `sidebar_view.rs`'s `mod tests` (`sidebar_view.rs:1760`) and
`chrome_tests.rs`:

- the three tiers are distinguishable **without color**: a workspace label seg
  is bold, a folder label seg is not, and each carries its own lead glyph;
- `row_bg` bands `Workspace`/`TerminalHost` and does **not** band `Folder`;
- with dividers on, a second workspace's placement has `lead_gap == 1` and its
  `height` is one greater than the same row with dividers off;
- with `dividers = false`, `sidebar_geom(...).heights` is **element-wise equal**
  to the pre-change heights — the "byte-identical layout" clause;
- gaps are suppressed in rail mode and while `sidebar_filter` is non-empty /
  `sidebar_filtering` is set;
- the gap counts in scroll geometry: `max_sidebar_scroll` and
  `sidebar_window`'s hidden tallies both grow by the number of gaps;
- `chrome_tests.rs:1012-1023`'s existing full-height hit-coverage assertion
  still passes with a gapped model (extend it to a two-workspace model);
- a caret-column click on a gap line does **not** toggle collapse, while the
  same column on the label line does.

Scoped commands only — never a full-workspace gate:
`just quick thegn-core` / `just quick thegn-host`, then
`cargo nextest run -p thegn-core config_example`,
`cargo nextest run -p thegn-host sidebar`.

**Frame change for e2e:** every baseline that shows the sidebar moves —
`test/muse/snapshots/sidebar__focused`, `chrome_regions__chrome`,
`responsive_breakpoints__layout`, `panel_*`, `themes__*`, `glitch_hunt_*`. Note
it in the commit body and the CHANGELOG; re-record with `just e2e-update` in a
later, deliberate pass. **Do not run e2e in this change.**

## 9. Chunks

Two chunks, **serial** — chunk 2 will not compile until chunk 1's config field
exists.

| #   | scope                                                                | files                                                                                                                                                                                                                                              | runs          |
| --- | -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------- |
| 1   | `[ui] sidebar_dividers` key + docs                                   | `crates/thegn-core/src/config_ui.rs`, `config/config.toml.example`                                                                                                                                                                                 | first         |
| 2   | tiering, gap layout, paint/hit wiring, help + CHANGELOG + spec delta | `crates/thegn-host/src/sidebar_view.rs`, `crates/thegn-host/src/chrome_tests.rs`, `crates/thegn-host/src/handlers/sidebar_mouse.rs`, `docs/help/sidebar.md`, `CHANGELOG.md`, `openspec/changes/add-sidebar-visual-hierarchy/specs/sidebar/spec.md` | after chunk 1 |

File sets are disjoint; the dependency is the `UiConfig` field, not a shared
file.
