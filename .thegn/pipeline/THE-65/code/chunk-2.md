# Chunk 2 — the `Alt-u` overlay, the heading vocabulary, and degradable bars

Read `.thegn/pipeline/THE-65/architect/design.md` first (§3.2, §3.3, §3.6, §5).
Work in `/home/blake/.superzej/worktrees/thegn/tg-the-65-usage-panel`.

## Dependency / overlap

- **Depends on chunk 1** (`thegn_core::usage_view`) — it will not compile until
  chunk 1 is committed. Start after chunk 1 lands.
- **File-disjoint from chunk 3** and may run in parallel with it. Chunk 3 calls
  `panel::sections::bar_segs`, whose **signature you must not change**; it also
  reads `usage_view`, which you must not change either.
- Do **not** touch `crates/thegn-host/src/panel/sections/usage.rs` or
  `docs/help/ai-usage.md` — those are chunk 3's.
- Do **not** edit anything under `openspec/`.

## Files touched (exact)

- `crates/thegn-host/src/detail/usage_dash.rs` — the rewrite
- `crates/thegn-host/src/sections.rs` — `HeadingToned.label_tone`; route
  `Cell::Bar` through the caps helper
- `crates/thegn-host/src/detail/status_modal.rs` — pass `Tok::Slot(S::Dim)` at
  its existing `HeadingToned` constructions (mechanical, no visual change)
- `crates/thegn-host/src/caps.rs` — new `bar_track` helper
- `crates/thegn-host/src/panel/sections/mod.rs` — `bar_segs` body only, one line
- `crates/thegn-host/src/detail.rs` — the `usage_overlay` / `apply_usage`
  re-exports at `:234` if their signatures change
- `crates/thegn-host/src/run.rs` — **parameter-only** edits at the four call
  sites (`:10544`, `:10598`, `:17000`, `:19364`). No new logic in `run.rs`.
- `crates/thegn-host/src/detail_tests.rs` — only if an existing test breaks

## Approach

### A. `caps::bar_track` (design §3.6)

Add to `crates/thegn-host/src/caps.rs` — the glyph chokepoint, and the one file
the glyph ratchet exempts (`platform_ratchet_tests.rs:76`):

```rust
/// A `(bar, track)` pair that degrades: the precision eighth-block gauge on a
/// UTF-8 terminal, `GlyphSet::bar_fill`/`bar_empty` on an ASCII one.
pub fn bar_track(frac: f32, w: usize) -> (String, String)
```

- `UnicodeLevel::Full | Basic` → delegate to `thegn_core::viz::bar_track`
  **verbatim**, so output is byte-identical to today and nothing on a UTF-8
  terminal moves.
- `UnicodeLevel::Ascii` → `g.bar_fill.repeat(filled)` /
  `g.bar_empty.repeat(w - filled)` with `filled = (clamp01(frac) * w).round()`,
  exactly as `loading/plan.rs:89-96` does.
- **Invariant:** `bar.chars().count() + track.chars().count() == w` on every
  branch — `draw_table` sizes its column on `w` (`sections.rs:576-580`) and a
  short bar shifts every column after it (design §5.8). Unit-test it across
  `frac` in `0.0..=1.0` and both unicode levels
  (`caps::test_override::unicode` already exists — see `caps.rs:103`, `:241-247`
  for the pattern).

Then route the two shared draw sites through it:

- `sections.rs:576-580`, the `Cell::Bar` arm of `draw_table`;
- `panel/sections/mod.rs:121-124`, `bar_segs` — body only, signature unchanged.

### B. `Section::HeadingToned` gains a label tone (design §3.3)

`draw_section` draws every heading label with `Tok::Slot(S::Dim)`
(`sections.rs:373-389`), so an account name cannot outrank a metric row today.
Add `label_tone: Tok` to `HeadingToned` and draw the label with it, **bold**
(`Sparkrow` already bolds at `sections.rs:429`, so the attribute is established).
Every pre-existing construction passes `Tok::Slot(S::Dim)` and is unchanged on
screen; only `detail/usage_dash.rs` and `detail/status_modal.rs` construct it.

Keep `Section::height` correct — `HeadingToned` stays height 1
(`sections.rs:194`). A block whose drawn height disagrees with its reported
height makes the tail of a scrolled stack unreachable (`sections.rs:14-17`).

### C. Rewrite `usage_sections` (design §3.2)

Replace the per-account `heading → facts grid → window table → sparkrow-per-
window` emission (`usage_dash.rs:338-369`) with a projection of
`thegn_core::usage_view::build(...)` with `peak_only: false`:

Top: one `Section::Heading { label: "usage", note: Some("<N accounts> · worst first") }`
(join with `caps::glyph(Glyph::Middot)`).

Per account, in `usage_view` order:

1. `Section::HeadingToned` — label = `AccountView::label`, `label_tone` from
   `AccountView::tone` (`None` ⇒ `Tok::Slot(S::Dim)`), note = `AccountView::note`
   toned the same way. This makes the worst account both first and loudest.
2. `Section::Table` of the metric rows — cells:
   `Text(name, Dim)`, `Bar(frac, BAR_W, tone_tok)`, `Text(pct, Text)`,
   `Text(resets, Dim)`, `Text(forecast, Ghost)`. Names arrive pre-padded to
   `view.name_w`, so every account's bar and `%` land in the same column
   (design §1.3). Keep `BAR_W = 16` (`usage_dash.rs:24`).
3. The facts line, **below** the numbers (design §1.4): one
   `Section::Heading { label: facts, note: None }`, skipped when `facts` is
   empty. The old `account_facts` 2-column `Grid` goes away.
4. A `Section::Sparkrow` **only for windows whose `forecast` is non-empty** —
   this is the §1.2 fix. Keep sourcing the series from
   `current_run(history[history_key])` as `trend_row` does
   (`usage_dash.rs:254-260`) and keep the ≥2-point guard: an empty sparkline is
   worse than no sparkline.
5. `sections::spacer()` between accounts — between blocks, not a top margin, and
   not a trailing one (design §1.1).

Then the token rollup block (`token_sections`, unchanged — its "host-wide"
heading is load-bearing, `usage_dash.rs:277-294`), then a final dim
`Section::Heading` carrying `usage_view::legend()` joined with
`caps::glyph(Glyph::Middot)`.

`ov.hint` is **not** a footer for a `Sections` popup — `detail.rs:925-927` never
reads it (design §5.4). The legend must be a `Section`.

The empty-accounts note (`usage_dash.rs:339-344`) stays exactly as it is.

### D. Honour the configured thresholds (design §1.6)

Delete `usage_dash::tone_tok`'s hard-wired `usage::tone` call (`:144-150`). Tone
comes from `MetricRow::tone`, which `usage_view` computed from the caller's
thresholds. Thread them in: `usage_overlay` and `apply_usage` take one extra
argument (`&thegn_core::config::UsageConfig`, or a `(warn, crit)` pair — your
call, but be consistent). All five call sites already have `model` in scope, so
each edit is `+1` argument, `&model.usage_cfg`:

`detail.rs:2097`, `run.rs:10544`, `run.rs:10598`, `run.rs:17000`, `run.rs:19364`.

`usage_loading` is unaffected.

## Tests

Keep the existing `usage_dash.rs` tests passing (adapt their assertions to the
new section order — that is expected), and add:

1. **Order and hierarchy** — three accounts, one over `crit_percent`: it is the
   first `HeadingToned` emitted and its `label_tone` is not the dim slot; an
   `Unavailable` account is last.
2. **Alignment** — two accounts with differently-wide window names: every
   `Cell::Text` name in every account's table has the same display width.
3. **Density** — a window with history but **no** forecast emits **no**
   `Sparkrow`; one with a forecast emits exactly one. Pin the total section
   count for a 2-account/2-window payload so a regression that re-adds a row per
   window fails here.
4. **Facts placement** — the facts line follows the table, is one row, and is
   absent for a bare account.
5. **Legend** — the last section is a `Heading` whose label contains a legend
   part from `usage_view::legend()`.
6. **Thresholds** — the same payload rendered with `warn_percent = 60` and with
   the defaults produces different bar tones (the §1.6 regression pin).
7. **Spacer** — a blank section sits between accounts and not after the last.
8. `caps::bar_track` — the width invariant on both unicode levels, and that the
   ASCII branch emits no `U+2500..U+259F` char.

`fill_ignores_other_overlays` (`usage_dash.rs:422`) and
`refresh_open_only_touches_the_status_modal` (`detail_tests.rs:1351`) must stay
green — the title guard is unchanged.

## Commands to run (scoped — nothing full-workspace)

```sh
just quick thegn-host
cargo nextest run -p thegn-host usage
cargo nextest run -p thegn-host sections
cargo nextest run -p thegn-host status_modal
cargo nextest run -p thegn-host glyph_literals_go_through_active_glyphs
```

Do **not** run `just test`, `just ci`, `just coverage` or `just e2e`. No usage
frame appears in any of the 17 muse baselines, so **no snapshot re-record is
needed** (design §4).

## Done criteria

- The overlay renders account-grouped, worst-first, one aligned line per metric,
  facts below the numbers, a spacer between accounts and a legend footer.
- Bars degrade on `UnicodeLevel::Ascii`; UTF-8 output is byte-identical to
  before.
- `test/glyph-literal-ratchet.txt` and `test/color-literal-ratchet.txt` are
  **unchanged** — needing an entry means a literal was written at a draw site
  instead of going through `caps` (design §4).
- All five commands above green; no `openspec/` file touched; no file belonging
  to chunk 3 touched.
- Committed on `tg/the-65-usage-panel` with **exactly** this subject:

```
feat(usage): scannable Alt-u overlay — grouped, aligned, legended (THE-65)
```
