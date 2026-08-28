# THE-64 — chunk 2 completion: header tiers + workspace separator gaps

Branch `tg/the-64-sidebar-distinction`, commit `149bb700`
(`feat(sidebar): tier headers and separate workspaces (THE-64)` — exact subject
per the chunk spec).

## What was done

**Part A — the three tiers** (`crates/thegn-host/src/sidebar_view.rs`):

- `compose_row_lines`, `Workspace | TerminalHost` arm: label is now
  `seg(Tok::Slot(S::Accent), …).bold()`; the new `else` arm pushes
  `seg(Tok::Slot(S::Accent), format!("{} ", gl.diamond_filled))` after the
  caret for a plain git workspace; the existing `dir` arm keeps `gl.dir` but
  moves `S::Text` → `S::Accent`; the `TerminalHost` arm keeps its
  `gl.host_local`/`gl.host_remote` glyph in `S::Dim` (local-vs-remote meaning,
  not tier) while its label takes the accent+bold treatment.
- `RowKind::Folder` arm: glyph `S::Dim` → `S::Faint`; label drops `.bold()`
  and splits into `seg(Tok::Slot(S::Text), row.label)` plus, when
  `child_count > 0`, `seg(Tok::Slot(S::Faint), " (n)")`; the single
  `format!` of the whole label is gone.
- `row_bg`: `RowKind::Folder` removed from the `header` predicate (falls
  through to `S::Panel`); doc-comment updated (workspace/host only).
- `SectionHeading` untouched. No `GlyphSet` field, no new theme slot, no
  literal at any draw site, no ratchet-file changes.

**Part B — the separator gap** (same file):

- `SidebarDisplay` gains `pub dividers: bool`, `Default = true`,
  `from_ui` reads `ui.sidebar_dividers` (chunk 1's config field).
- Private helpers `lead_gap_rows(model, visible, i)` + `dividers_on(model)`
  added verbatim per the spec — the single source for the height pass
  (`sidebar_geom_from` now does `h += lead_gap_rows(...)`), the compose pass
  (`build_sidebar` inserts that many `Line::Blank` at the head, before the
  lockstep `debug_assert_eq!`), scroll geometry (`max_sidebar_scroll` /
  `sidebar_window` read the heights, so they count gaps for free) and
  hit-testing.
- Clipped-tail trim generalized from `SectionHeading && i > 0` to
  `lead_gap > 0`; the placement's `lead_gap` is recomputed after the trim as
  the count of leading blanks still in `lines`, so a partly-fitting row drops
  its gap and both paint and hit-testing see a gap-less row.
- `SidebarPlacement` gains `pub lead_gap: usize`; `draw_sidebar` splits its
  single `draw_lines` into a gap call on `Tok::Slot(S::Panel)` and a body call
  on `p.bg` (the gap must not paint on the row's own band or two collapsed
  workspaces merge into one tall band); the cursor-bar loop starts at
  `p.lead_gap`.
- `RowHit` gains `pub lead_gap: usize`, populated from the placement;
  `caret_x` columns unchanged (workspace `rect.x + 4`, folder `rect.x + 3`).

**Part C — caret guard** (`crates/thegn-host/src/handlers/sidebar_mouse.rs`,
the only edit there): the press path's collapse toggle is gated on
`my >= hit.y + hit.lead_gap` per the spec, so the caret column on a gap line
is inert.

**Part D — docs**:

- `docs/help/sidebar.md`: new "Reading the tree" section (tiers + the
  `[ui] sidebar_dividers` opt-out). No action ids touched — help ratchets
  unaffected (frontmatter unchanged).
- `CHANGELOG.md`: "Changed — the sidebar reads in tiers, and repos no longer
  run together" under `[Unreleased]`, stating plainly that e2e baselines have
  NOT been re-recorded.
- `openspec/changes/add-sidebar-visual-hierarchy/specs/sidebar/spec.md`: the
  "not a click target / resolves as empty space" clause and the
  "The gap is interaction-transparent" scenario amended to the built behavior
  (click resolves to the header it precedes; caret cell inert on the gap;
  drop unchanged). Requirement/Scenario shape intact for
  `openspec validate --all --strict`. The other three scenarios untouched.

## Tests added

`sidebar_view.rs` `mod tests`: `header_tiers_are_distinguishable_without_color`
(seg-level slot+bold assertions, no resolved colors),
`only_the_project_tier_is_banded` (`row_bg` slots),
`dividers_gap_the_boundary_between_workspaces` (gap + off⇒all-ones heights),
`gaps_are_suppressed_in_rail_and_while_filtering`,
`gaps_count_in_scroll_geometry` (max_scroll + hidden_below grow by the gaps),
`a_clipped_gapped_workspace_keeps_its_label`. Helper `two_ws_rows()`.

`chrome_tests.rs`: sibling `two_workspaces()` helper (many_rows untouched);
digit-assertion updates for the new diamond (workspace lead glyph now sits
between caret and label); `build_sidebar_and_click_hit_test_round_trip`
extended with a two-workspace gapped model (every screen line of every
placement, gap included, resolves to its own `visible_index`).

`sidebar_mouse.rs`: `a_click_on_the_gap_line_does_not_toggle_collapse` —
caret column on the gap line never toggles, same column on the label line
does; DB persist isolated via `crate::testenv::EnvVarGuard` (`XDG_STATE_HOME`
to a throwaway dir), per the crate rule against touching the live DB in
tests.

## Verification (scoped only, per dev-loop policy)

- `just quick thegn-host` — clean (clippy, lib/bin).
- `cargo nextest run -p thegn-host sidebar` — **211/211 passed**.
- `cargo nextest run -p thegn-host chrome` — **86/86 passed**.
- Pre-commit hook ran treefmt (it reformatted 3 files' line joins; cosmetic
  only, folded into the commit), shellcheck/yamllint n/a.
- `git status` clean; exactly the six files in the chunk spec were touched;
  no `test/*-ratchet.txt` changed.

## Unverified (for the review stage)

- **e2e NOT run** (mandated by the chunk spec and the Lead). Every muse
  baseline showing the sidebar moves (`sidebar__*`, `chrome_regions__chrome`,
  `responsive_breakpoints__layout`, `panel_*`, `themes__*`,
  `glitch_hunt_*`); re-record with `just e2e-update` in a later deliberate
  pass. Noted in the commit body and the CHANGELOG.
- **Full suite not run** — no `just test` / `just ci` / `just coverage`
  (heavy-guard policy; the pre-push hook owns those). Tests outside the
  `sidebar`/`chrome` filters in `thegn-host` and other crates compiled only
  where the scoped nextest filters pulled them in. In particular
  `cargo nextest run -p thegn-core config_example` was NOT re-run here —
  chunk 2 touches no `thegn-core` code, and chunk 1 already landed that test.
- **Full clippy (including test targets) not run** — `just quick thegn-host`
  covers lib/bin only. Clippy on the new test code runs at pre-push.
- **`just term-check` (mono/16-color legibility) not run** — the ladder is
  asserted at the seg level (weight/glyph/indent, quantize-losslessly) and
  the design argues it survives; the six-environment render check itself
  runs in CI only.
- The openspec delta was shape-checked by hand (headers intact) but
  `openspec validate --all --strict` was not executed (runs in `just ci`).
