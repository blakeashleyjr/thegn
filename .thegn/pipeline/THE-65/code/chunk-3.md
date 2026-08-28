# Chunk 3 — the System ▸ Usage panel section, and the help page

Read `.thegn/pipeline/THE-65/architect/design.md` first (§3.5, §4, §5).
Work in `/home/blake/.superzej/worktrees/thegn/tg-the-65-usage-panel`.

## Dependency / overlap

- **Depends on chunk 1** (`thegn_core::usage_view`) — will not compile until
  chunk 1 is committed. Start after chunk 1 lands.
- **File-disjoint from chunk 2** and may run in parallel with it. Chunk 2 is
  editing `panel/sections/mod.rs` (the body of `bar_segs`) and `sections.rs`;
  **do not touch either**. Keep calling `bar_segs` with its current signature —
  chunk 2 makes it degrade, transparently to you.
- Do **not** edit anything under `openspec/`.

## Files touched (exact)

- `crates/thegn-host/src/panel/sections/usage.rs` — the rewrite
- `docs/help/ai-usage.md` — the prose

Nothing else. If you find yourself needing a fourth file, stop and say so.

## Approach

### A. Project the shared model (design §3.5)

`content()` (`panel/sections/usage.rs:119-195`) currently decides ordering (none
— discovery order), naming (`w.label` raw), tone and layout for itself. Replace
those decisions with one `thegn_core::usage_view::build(...)` call so a window
that reads `7-day window 94%` in the panel reads identically in the `Alt-u`
overlay. This is the shared-list-fn lesson from the panel audits.

The three width tiers survive as `ViewOpts` and projection choices — the module
doc-comment at `usage.rs:5-12` describes what each tier answers and must stay
true:

- **Normal** (`!ctx.deep()`) — `ViewOpts { peak_only: true }`: the account
  heading plus its single worst metric row. Drop the hand-rolled
  `a.peak_window()` branch at `:181-185`; `usage_view` does that selection now
  (and it must keep using `peak_window()`, not `windows.first()` — the existing
  test `resting_width_shows_the_window_nearest_its_limit` at `:296-308` is the
  pin; keep it).
- **Half** (`ctx.deep() && !ctx.full()`) — every metric row, indented 2, aligned.
- **Full** — the above plus the facts line, the token block and the legend.

Concretely:

1. Accounts iterate in `usage_view` order, worst first. **Do not sort
   `ctx.model.usage` in place** — `usage_view::order` returns indices for
   exactly this reason (design §5.1): `usage::peak_across` indexes that slice for
   the statusbar badge (`usage.rs:311-323`) and the alert handler keys off it.
2. The account heading gets the peak window's hue on the account name (not just
   on the plan chip), so the worst account is both first and loudest. Tones come
   from `MetricRow::tone` / `AccountView::tone` — keep using `hue(...)` and the
   existing `Tone` mapping (`usage.rs:27-38`); no colour literal at a draw site.
   `state_note` (`:48-60`) is superseded by `AccountView::note`, which carries
   the same four cases including `"no windows reported"` — delete the local
   helper and move its test to assert on the view.
3. Metric rows use `MetricRow::name` (already padded — drop the local
   `format!("{:<8}", w.label)` at `:70`), `frac`, `pct`, `resets`, and the
   `forecast` tail. Keep `BAR_W` / `BAR_W_DEEP` (`:22-23`) and keep calling
   `bar_segs`.
4. `forecast_row` (`:238-250`) collapses into the metric row's tail
   (`runs out in 3h 12m`) — one line per metric, not two. The local
   `crate::detail::history_key` call at `:242` goes away; `MetricRow` carries
   the key.
5. `fact_rows` (`:93-117`) collapses to the single `AccountView::facts` line at
   Full width, rendered **after** the metric rows.
6. At Full width, append the legend: `usage_view::legend()` joined with
   `crate::caps::glyph(Glyph::Middot)`, as a dim row above the existing
   `hint_row` (`:193`). Do not add a legend at Normal/Half — there is no room.
7. `token_rows` (`:255-287`), `proxy_spend_rows` (`:200-233`), the empty-state
   block (`:126-146`) and the `hint_row` all stay as they are.

`ctx.model.usage_cfg.warn_percent / crit_percent` feed `ViewOpts` — the panel
already honours them (`:27-34`) and must keep doing so.

### B. Help page (`docs/help/ai-usage.md`)

The page already claims `actions: [open-usage]` and `contexts: [panel:usage]`
(`:6-7`); **no new action id and no new key**, so no ratchet file changes. But
the help-prose ratchet requires the page to actually describe what the surfaces
show, and lines 17-30 currently describe the old layout.

Update:

- the `Alt-u` bullet (`:23-25`) — accounts are grouped and listed **worst first**
  (the account nearest a limit at the top), one aligned line per limit with its
  bar, used percent and reset countdown, identity facts summarised on one line
  below the numbers, and a legend on the last row;
- the **System ▸ Usage** bullet (`:26-30`) — keep the three width tiers, but say
  that the resting width shows each account's worst limit, and that names read
  in plain language (`7-day window`, `5-hour window`) rather than provider
  shorthand;
- add a sentence that the ordering is by how close each account is to a limit,
  so the list is a ranking, not a fixed roster — a reader who expects a stable
  order needs to be told;
- note that the same warn/critical thresholds colour **all three** surfaces
  (this is now true; it was not before — design §1.6).

Leave the `## Token counts are host-wide` (`:119-127`) and
`## Model-proxy spend` (`:129-144`) sections alone; both still describe reality.
Do not invent a key binding the code does not have — there is no expand/collapse
in this change (design §3.4).

## Tests

In `crates/thegn-host/src/panel/sections/usage.rs`'s `#[cfg(test)] mod tests`:

1. Keep `resting_width_shows_the_window_nearest_its_limit` (`:296-308`) green.
2. Rewrite `state_note_explains_every_non_ok_row` (`:311-325`) against
   `AccountView::note` — all four cases (`Ok` with windows → none,
   `Ok` without → `no windows reported`, `Loading` → `…`, `Unavailable` →
   `unavailable: token expired`).
3. **Order** — a `Normal`-width render of three accounts puts the one nearest
   its limit on the first account row and an `Unavailable` one last.
4. **Tier shape** — `Normal` renders exactly one metric row per account;
   `Half` renders one per window; `Full` adds the facts line and the legend row.
5. **Alignment** — at `Half`, two accounts with differently-wide window names
   have their bars starting at the same column.
6. **Plain language** — a 300-minute window renders `5-hour window`, not `5h`.
7. **Forecast** — a forecasting window's row carries `runs out in` on the **same
   row** as the bar, and no extra row is emitted.

Use the existing `render(Section::Usage, PanelWidth::…)` test harness in
`panel/sections/mod.rs`'s test module (see `full_views_carry_the_overlay_signatures`
at `:1685`) rather than building a new one.

## Commands to run (scoped — nothing full-workspace)

```sh
just quick thegn-host
cargo nextest run -p thegn-host usage
cargo nextest run -p thegn-host help
```

The `help` filter covers the three help ratchets
(`crates/thegn-host/src/help/ratchet_tests.rs`) — they must be green **without**
editing `test/help-ratchet.txt`, `test/help-prose-ratchet.txt` or
`test/help-context-ratchet.txt`. Those files are shrink-only; needing to add a
line means something went wrong.

Do **not** run `just test`, `just ci`, `just coverage` or `just e2e`. No usage
frame appears in any of the 17 muse baselines, so no snapshot re-record is
needed (design §4).

## Done criteria

- The panel section renders worst-first, plain-language, aligned, one line per
  metric, with the facts line and legend at Full width — and its numbers, names
  and colours match the overlay's, because both come from `usage_view`.
- `docs/help/ai-usage.md` describes the surfaces as they now behave; the three
  help ratchet files are unchanged.
- All three commands above green; no `openspec/` file touched; no file belonging
  to chunk 2 touched.
- Committed on `tg/the-65-usage-panel` with **exactly** this subject:

```
feat(usage): panel Usage section shares the scannable layout (THE-65)
```
