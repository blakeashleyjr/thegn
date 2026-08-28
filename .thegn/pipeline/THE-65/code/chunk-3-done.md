# Chunk 3 done — the System ▸ Usage panel section, and the help page

Commit: `fb1e1ea3` — `feat(usage): panel Usage section shares the scannable layout (THE-65)`
Branch: `tg/the-65-usage-panel`. Files touched: exactly the two in the spec
(`crates/thegn-host/src/panel/sections/usage.rs`, `docs/help/ai-usage.md`); no
`openspec/` file, no chunk-2 file (`detail/usage_dash.rs`, `sections.rs`,
`detail/status_modal.rs`, `panel/sections/mod.rs` all remain untouched by me —
verified with `git show --stat HEAD`).

## A. The panel section (`panel/sections/usage.rs`)

- `content()` now builds one `usage_view::build(...)` view from
  `ctx.model.usage` + `ctx.model.usage_history` with
  `ViewOpts { now, warn_percent, crit_percent, peak_only: !ctx.deep() }` —
  ordering (worst first), plain-language names, tones, the shared name-column
  width and the reset/forecast phrases all come from core, so the panel and the
  `Alt-u` overlay read identically. The input slice is never reordered; the
  view carries indices/order only (statusbar badge + alert handler still key
  off discovery order).
- **Normal** (`!ctx.deep()`): heading + the account's single worst metric row
  (`peak_only: true` → `peak_window()`, per the pinned test). **Half**: every
  window, indented 2, bars aligned on the shared padded name column. **Full**:
  plus the one-line `AccountView::facts` BELOW the metric rows, the token
  block, and the legend.
- Heading: the account name takes the peak window's hue (`AccountView::tone` →
  existing `Tone`/`hue()` mapping; dim when `None`); the plan stays a teal chip
  for Ok accounts (the view's note IS the plan there); non-Ok notes render dim.
- Metric rows: `MetricRow::name` (already padded — the local
  `format!("{:<8}", …)` is gone), `frac`/`pct`/`resets`/`forecast` tails on ONE
  line (`runs out in 3h 12m` follows `resets in …`); `forecast_row` and its
  `crate::detail::history_key` call are deleted (`MetricRow::history_key`
  replaced them; byte-identical to `format!("{key}#{label}")`).
- Deleted local helpers superseded by the view: `state_note` (→
  `AccountView::note`), `resets_in`, `fact_rows` (→ `AccountView::facts`).
- Kept as-is: `BAR_W`/`BAR_W_DEEP` + `bar_segs` (current signature — chunk 2's
  degrade lands transparently), the empty-state block, `hint_row`, the full
  width leading `rule()`, the deep-width blank between accounts,
  `proxy_spend_rows`, `token_rows`. Legend at Full only: `usage_view::legend()`
  joined with `caps::glyph(Glyph::Middot)`, dim, directly above the hint row.
- Thresholds: `usage_cfg.warn_percent/crit_percent` feed `ViewOpts`, so
  panel/overlay/badge tone from the same configured numbers (design §1.6).

## B. Help page (`docs/help/ai-usage.md`)

- `Alt-u` bullet now describes the new overlay: worst first, one aligned line
  per limit with bar + used % + reset countdown, facts on one line below the
  numbers, legend on the last row.
- System ▸ Usage bullet keeps the three width tiers, says the resting width
  shows each account's worst limit and names read in plain language
  (`7-day window`, `5-hour window`).
- Added: the list is a ranking by closeness to a limit, not a fixed roster; and
  the same `warn_percent`/`crit_percent` thresholds colour all three surfaces.
- `## Token counts are host-wide` and `## Model-proxy spend` untouched; no
  invented key binding; frontmatter (`actions: [open-usage]`,
  `contexts: [panel:usage]`) unchanged.

## Tests (in `usage.rs`'s `mod tests`, per the spec)

1. `resting_width_shows_the_window_nearest_its_limit` — kept, green.
2. `view_note_explains_every_account_state` — rewritten against
   `AccountView::note` via `usage_view::build`: Ok+windows → `""` (plan),
   Ok-without → `no windows reported`, Loading → `…`, Unavailable →
   `unavailable: token expired`.
3. `normal_width_lists_the_account_nearest_its_limit_first` — Normal render of
   three accounts (91% / 40% / Unavailable) puts 91% first, Unavailable last.
4. `width_tiers_grade_the_detail_peak_all_and_full` — Normal: one metric row
   per account; Half: one per window; Full: same rows + facts line + legend,
   with facts positioned after the metric rows.
5. `half_width_bars_line_up_across_differently_named_windows` — bar-start
   column equal across accounts with differently-wide window names.
6. `windows_read_in_plain_language` — a 300-minute window renders `5-hour
window` and no row contains the provider shorthand `5h`.
7. `forecast_lives_on_the_metric_row_not_beside_it` — a rising two-sample
   history yields exactly one `runs out in` row and it is the bar's row.

Harness note: the shared `render(Section, PanelWidth)` helper lives in
`panel/sections/mod.rs`'s private `mod spec`, which a sibling owns and I may
not edit — so the tests mirror it locally (same `FrameModel`/`PanelUi`/
`SectionCtx` shape, same per-width `(cols, rows)` as `mod spec::render`, the
pattern already used by `changes.rs`/`problems.rs`/`symbols.rs` tests). The
gauge alphabet used to locate bar columns is derived by sweeping
`caps::bar_track` across a cell boundary — no glyph literal in test code
(the glyph and caret ratchets scan tests too; both caught the first draft).

## Commands run (all scoped, nothing full-workspace)

- `just quick thegn-host` — clean.
- `cargo nextest run -p thegn-host usage` — 25/25 pass (my 7 + the existing
  statusbar-badge usage tests).
- `cargo nextest run -p thegn-host help` — 71/71 pass.
- `cargo nextest run -p thegn-host ratchet` — 12/12 pass (glyph-literal,
  caret, platform, color ratchets), after deriving the test's gauge alphabet
  through `caps::bar_track`. The three help ratchets are also in the `help`
  filter run above; `test/help-ratchet.txt`, `test/help-prose-ratchet.txt`,
  `test/help-context-ratchet.txt` unchanged.
- Pre-commit treefmt reformatted the file (import order, line joins); tests
  re-run green after the reformat before committing.

Not run, per instructions: `just test` / `just ci` / `just coverage` / e2e.

## Unverified

- `just coverage` (95% core gate) — not run (heavy gate, per policy). My
  changes are host-side; `usage_view.rs` (core) was chunk 1's and is unchanged
  by this chunk.
- e2e snapshots — not run (per spec: no usage frame in any of the 17 muse
  baselines, so no re-record needed); verified by grep by the architect, not
  re-verified here.
- Interaction with chunk 2's final `bar_segs` degrade body — I compile and
  test against the in-flight working tree (which already routes `bar_segs`
  through `caps::bar_track`), but chunk 2's files may still change under it;
  the contract I rely on is the unchanged `bar_segs(frac, w, fg)` signature.
- The full help-page prose re-read by a human eye (ratchet asserts mentions,
  not readability).
