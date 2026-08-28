# Chunk 2 — done: the `Alt-u` overlay, the heading vocabulary, and degradable bars

Branch `tg/the-65-usage-panel`, commit **`e34e0cee`**
(`feat(usage): scannable Alt-u overlay — grouped, aligned, legended (THE-65)` —
exact subject from the chunk spec, pre-commit treefmt hook green).

## What landed

- **`caps::bar_track(frac, w)`** (`crates/thegn-host/src/caps.rs`) — the gauge
  chokepoint. `Full | Basic` delegates to `thegn_core::viz::bar_track`
  verbatim (byte-identical UTF-8 output); `Ascii` fills `GlyphSet::bar_fill` /
  `bar_empty` (`=`/`-`) exactly as `loading/plan.rs` does. Both routes keep
  `bar + track == w`. Also re-exports `thegn_core::termcaps::Glyph` as
  `pub use crate::caps::Glyph` — chunk 3's in-flight `panel/sections/usage.rs`
  imports the token type through the chokepoint; one line in my own file.
- **`Section::HeadingToned` gains `label_tone: Tok`** (`sections.rs`), drawn
  **bold** (the established `Sparkrow` attribute). The only two pre-existing
  constructors — `detail/status_modal.rs` (daemon, sessions) and
  `usage_dash::token_sections` — pass `Tok::Slot(S::Dim)`.
- **`Cell::Bar` arm of `draw_table` and `bar_segs` routed through
  `caps::bar_track`** (`sections.rs`, `panel/sections/mod.rs` body-only —
  signature unchanged, chunk 3's contract honoured).
- **`usage_sections` rewritten** (`detail/usage_dash.rs`): a projection of
  `usage_view::build(..., peak_only: false)` — top `usage` heading with
  `<N accounts> · <middot> worst first`; per account, in `usage_view` order:
  toned `HeadingToned` (label + note both carry the peak tone; `None` ⇒ dim),
  one metric `Table` whose names arrive pre-padded to the shared `name_w`
  (cells: name/bar/pct/resets/forecast), a `Sparkrow` **only where a forecast
  exists** (sourced from `current_run(history[history_key])`, ≥2-point guard
  kept), the one-row facts line **below** the numbers, and a `spacer()` between
  accounts only; then the unchanged `token_sections`, then a trailing dim
  legend `Heading` = `usage_view::legend()` joined with `caps::glyph(Middot)`.
  `tone_tok` now maps `UsageTone` → `Tok` only — the hard-wired `usage::tone`
  call is gone (§1.6).
- **Thresholds threaded** (§1.6): `usage_overlay` / `apply_usage` take
  `&thegn_core::config::UsageConfig`; all five call sites pass `&model.usage_cfg`
  (`detail.rs:2097`, `run.rs:10544`, `:10598`, `:17000`, `:19364`) — parameter-only
  edits, no new logic in `run.rs`. `usage_loading` unaffected; title guard
  untouched.
- The empty-accounts note, the token block, and `history_key` are unchanged.

## Tests (all in-repo, scoped)

- `caps.rs`: width invariant across `0.0..=1.0` × widths × all three unicode
  levels; ASCII branch emits no `U+2500..U+259F` char and clamps out-of-range
  fractions; Unicode branch equals `viz::bar_track` byte-for-byte.
- `usage_dash.rs`: order/hierarchy (crit account first `HeadingToned`, red
  label, `Unavailable` last dim), alignment (shared 13-cell name column across
  accounts), density (pinned section count 7 for a 2-account/2-window payload;
  forecast-less window with history emits no sparkrow), facts placement
  (follows the table, one row, absent for a bare account), legend (last
  section, contains every `legend()` part, joined with the caps middot),
  thresholds (70% is amber at `warn_percent = 60`, green at the defaults),
  spacers (between accounts, never at the edges). Existing tests adapted to
  the new order; `fill_ignores_other_overlays` and the trend guard semantics
  kept (now additionally forecast-gated).

## Verification run (scoped, per dev-loop policy)

- `just quick thegn-host` — green.
- `cargo nextest run -p thegn-host usage` — 25/25.
- `cargo nextest run -p thegn-host sections` — 75/75.
- `cargo nextest run -p thegn-host status_modal` — 7/7 (includes
  `refresh_open_only_touches_the_status_modal`).
- `cargo nextest run -p thegn-host glyph_literals_go_through_active_glyphs` —
  green. `test/glyph-literal-ratchet.txt` and `test/color-literal-ratchet.txt`
  are **unchanged**; nothing under `openspec/` touched (`git diff` empty on
  both paths).
- Pre-commit hook (treefmt + shellcheck + yamllint) passed on the commit.

## Unverified (for the review stage)

- **Heavy gates not run** (lead addendum / dev-loop policy): no `just test`,
  `just lint`, `just coverage`, `just ci`, no full-workspace compile beyond
  `just quick thegn-host`'s lib/bin scope. The pre-push hook will carry the
  real gate.
- **No e2e** — forbidden by the chunk (design §4: no usage frame in any of the
  17 muse baselines, so no re-record needed); visual rendering was verified at
  unit level only, never on screen.
- **"Byte-identical on screen" for the pre-existing `HeadingToned` labels is
  the design's claim, not something I measured** (`design.md` §3.3): the old
  draw was dim without bold, the new one dim **with** bold, so the status
  modal's "daemon"/"sessions" and the token-block heading are bold-dim now. A
  reviewer may want one eyeball on the status modal (F9-style open) to confirm
  that reads as intended.
- **Shared-worktree races**: while chunk 3's sibling saved files mid-run,
  `panel::sections::usage::tests::width_tiers_grade_the_detail_peak_all_and_full`
  and the glyph ratchet each failed once and passed on immediate re-run; their
  files (`panel/sections/usage.rs`, `docs/help/ai-usage.md`) are **not** in my
  commit. My commit's tree pairs HEAD's `usage.rs` (which uses the unchanged
  `bar_segs` signature and constructs no `HeadingToned`) with my `sections.rs`
  — consistent by inspection, but not proven by a test run of that exact tree.
- The `caps::Glyph` re-export exists to unblock the sibling's in-flight import
  (`use crate::caps::Glyph`); cross-chunk integration is only as verified as
  the shared-tree test runs above.
