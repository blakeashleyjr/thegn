# Chunk 5 — Masthead widget, calendar-popup block, docs, spec reconcile

THE-46. Read `.thegn/pipeline/architect/design.md` §6.7, §6.8, §7 first, plus
`crates/thegn-host/src/detail/calendar/render.rs` (`clocks_table` is the
template for the new block) and `crates/thegn-host/src/chrome.rs:1387`
(`masthead_widget`).

Iterate with `just quick thegn-host`.

## Scope

Everything the user sees, plus the documentation and OpenSpec reconciliation
that closes the change out.

Reads `model.weather` / `model.weather_cfg` (chunk 4) and
`thegn_core::weather::*` (chunk 1).

## Files

| File                                                                             | Action                                                                                               |
| -------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `crates/thegn-host/src/chrome.rs`                                                | edit — `masthead_widget` `"weather"` arm, `fit_stats_cluster` shed order, `[bars] top_right` default |
| `crates/thegn-host/src/chrome_tests.rs`                                          | edit — widget + shed tests                                                                           |
| `crates/thegn-host/src/detail.rs`                                                | edit — pass `model` into `calendar::open`                                                            |
| `crates/thegn-host/src/detail/calendar/mod.rs`                                   | edit — `CalState` carries the reading; `preferred_cols`                                              |
| `crates/thegn-host/src/detail/calendar/render.rs`                                | edit — the `WEATHER` block                                                                           |
| `crates/thegn-core/src/config.rs`                                                | edit — one line: `"weather"` in `BarsConfig::default().top_right`                                    |
| `docs/help/bars.md`                                                              | edit                                                                                                 |
| `docs/help/calendar.md`                                                          | edit                                                                                                 |
| `tasks.md`                                                                       | edit — one roadmap row under group AM                                                                |
| `openspec/changes/add-weather-widget/{design,tasks}.md`, `specs/weather/spec.md` | edit — fold in the design deltas                                                                     |

**Shared file:** `chrome.rs` is also touched by chunk 4 (the `FrameModel`
fields). Your anchors are `masthead_widget`, `fit_stats_cluster` and nothing
else.

## Approach

### 1. The `weather` masthead widget

In `masthead_widget`, beside the `"date"` / `"clock"` arms:

```rust
// `[weather]`. Absent — not blank — when disabled, before the first reading,
// or once the reading is hard-expired: a weather widget that shows last
// Tuesday's sun is worse than no widget.
"weather" => {
    let snap = model.weather.as_ref()?;
    let cfg = &model.weather_cfg;
    let now = wall_clock().timestamp();
    let text = /* glyph + temp, see below */;
    match thegn_core::weather::freshness(
        snap.fetched_at, now, cfg.stale_after_secs, cfg.hard_expiry_secs,
    ) {
        Freshness::Expired => None,
        Freshness::Fresh => Some(w(text, col(S::Dim))),
        // Dimmer, not coloured: staleness is a quiet caveat, not an alert.
        Freshness::Stale => Some(w(text, col(S::Ghost))),
    }
}
```

Text: `format!("{glyph} {temp}")` where
`glyph = thegn_core::weather::sky_glyph(snap.sky, crate::caps::active_glyphs())`
and `temp = thegn_core::weather::fmt_temp(snap.temp, snap.units)`. When the
glyph is empty (`Sky::Unknown`), render the temperature alone with no leading
space. **No glyph literal at the draw site** — everything comes through
`active_glyphs()`.

### 2. Shed order and the shipped default

- `fit_stats_cluster`'s victim list becomes
  `["date", "weather", "uptime", "load", "freq", "swap", "temp", "disk", "gpu", "battery", "net", "clock", "mem", "cpu"]`.
  Extend the function's doc comment with the reasoning: `date` is softest (the
  clock carries the same information), and weather is next — the user opted
  in, so it should outlive `uptime`/`load`/`freq`, but it is not `cpu`.
- `BarsConfig::default().top_right` (in `crates/thegn-core/src/config.rs`,
  ~line 2970) gains `"weather".into()` immediately **before** `"date"`. It is
  inert until `[weather] enabled = true`, because the widget arm returns
  `None` with no snapshot — the same way `gpu` is inert on a machine with no
  GPU. Note that in a comment so nobody "fixes" it later.

### 3. The calendar popup's `WEATHER` block

`calendar::open` needs the reading. It is called from
`widget_detail(w, near, model, ctx)`, which already has `model` — so:

- `detail.rs:1911` becomes `calendar::open(ctx, near, model)`;
- `calendar::open(ctx, &super::StatusCtx, near, model: &FrameModel)` copies
  `model.weather.clone()` and the three fields it needs from
  `model.weather_cfg` into `CalState`. Keep the popup's founding invariant:
  snapshot at open time, hold no borrow across frames.

`CalState` gains:

```rust
/// The weather reading at open time, or `None` when `[weather]` is off /
/// nothing has landed. Refreshed by `retick_open` from the same model the
/// widget reads, so the block never disagrees with the masthead.
pub weather: Option<thegn_core::weather::WeatherSnapshot>,
/// The `[weather]` knobs the block needs: staleness thresholds, whether to
/// draw the day strip, and how many days of it.
pub wx: WxUiCfg,
```

with a small `WxUiCfg { stale_after_secs, hard_expiry_secs, show_forecast,
forecast_days }` beside `CalUiCfg` (put it in `calendar_docs.rs` next to
`CalUiCfg` if that reads better; either home is fine, but only one).

In `render.rs::sections_of`, **above** the `WORLD CLOCKS` block (weather is
"here, right now"; the clocks are "elsewhere, right now" — and the clocks stay
anchored at the bottom where existing users expect them):

```rust
if let Some(w) = weather_block(st) {          // returns None when absent
    out.push(super::super::spacer());
    out.push(w.heading);
    out.push(w.table);
}
```

- Heading: `format!("WEATHER {} {}", glyphs.middot, place)` — falling back to
  `"WEATHER"` alone when `place` is empty. `note:` is `None` when fresh and
  `Some(fmt_age(fetched_at, now))` when stale. Use `st.now.timestamp()` so the
  age tracks `retick_open` and never calls a clock at a draw site.
- Absent entirely — not an empty block — when there is no snapshot or the
  reading is `Freshness::Expired`. This mirrors how the agenda block is
  suppressed when `has_sources` is false.
- Body: a `Section::Table` (not `Grid` — the same reason `clocks_table` gives:
  a Grid gives one tone to the whole value string and flattens the row).
  Current-conditions row:

  | cell                                                     | tone                   |
  | -------------------------------------------------------- | ---------------------- |
  | `format!("{glyph} {description}")`                       | `Tok::Slot(S::Text)`   |
  | `fmt_temp(temp, units)`                                  | `Tok::Slot(S::Text)`   |
  | `format!("feels {}", fmt_temp(feels_like, units))`       | `Tok::Slot(S::Dim)`    |
  | `format!("H {} L {}", fmt_temp(hi,..), fmt_temp(lo,..))` | `Tok::Hue(Hue::Amber)` |
  | `format!("{}%", humidity_pct)`                           | `Tok::Slot(S::Faint)`  |
  | `fmt_wind(wind, units)`                                  | `Tok::Slot(S::Faint)`  |

  Then, when `wx.show_forecast` and the forecast is non-empty, up to
  `wx.forecast_days` rows: weekday (`%a`, `Tok::Slot(S::Dim)`), glyph, and
  `format!("{} / {}", fmt_temp(hi,..), fmt_temp(lo,..))`. `draw_table` sizes
  each column to its widest cell, so short rows collapse on their own — no
  explicit shedding needed (the note `clocks_table` already makes).

- `preferred_cols` in `mod.rs`: widen the `.max(...)` chain so the current-
  conditions row fits when weather is present. Do **not** widen it when
  weather is absent — the default popup width must not change, or every
  existing e2e baseline shifts.
- `content_height` recomputes from `sections_of`, so height is automatic; the
  existing clamp against `ctx.screen.rows` already covers a short terminal.
- The block is **not** clickable: `hit()`'s agenda detection keys off "the
  first `Section::Table` after the grid". Adding a table _after_ the clocks
  would be safe, but this one goes _before_ them — verify the agenda branch
  still selects the agenda's table and not the weather one. If the ordering
  makes that fragile, gate on `st.ui.show_agenda && st.ui.has_sources` as it
  already does and add a test.

### 4. Help pages

- `docs/help/bars.md`: add `weather` to the widget-id list (line ~30), and a
  short paragraph after the date/clock one: what it shows, that it is off by
  default and needs `[weather] enabled = true`, that clicking it opens the
  calendar, that it dims when stale and hides when very old, and that it sheds
  right after `date` as the terminal narrows.
- `docs/help/calendar.md`: extend the "three blocks" sentence to four when
  weather is on, add the block to the ASCII box diagram, and point at
  `[weather]` in the config.
- **No frontmatter change** in either page — this chunk adds no action id,
  chord, zone or panel context, so the help ratchets need no allowlist edits.
  (Confirm by running the ratchet test rather than assuming.)

### 5. Roadmap row

`tasks.md`, group **AM. Daily-driver / non-code tiles** (~line 1205):

```
- [x] 760. Weather in the date/time surfaces (THE-46) — optional `[weather]`, off by
       default: `core::weather` (pure decode/units/staleness/glyph classes),
       `svc::weather` seam (`wttr_in` keyless; `open_meteo`/`openweathermap`
       reserved), `weather` masthead widget + calendar-popup block, last-good
       snapshot cached in `ui_state`
```

Re-check the highest number in use before committing to `760` (`grep -oE
'^- \[.\] [0-9]+\.' tasks.md | grep -oE '[0-9]+' | sort -n | tail -1`); other
in-flight branches claim numbers too.

### 6. OpenSpec reconcile

The change `openspec/changes/add-weather-widget/` already exists and is
tracked. Fold the implemented reality back into it — the artifacts must not
drift from the code:

- `design.md`: replace the `weather_cache` table + `user_version` bump section
  with the `ui_state`-KV decision and its reasoning; replace "background lane,
  `Background` QoS" with the `spawn_blocking` decision and the `spawn_bg`
  drop-on-saturation reason; replace "pin under `THEGN_E2E=1`" with "forced
  off under the freeze, like `[usage]`"; resolve the three open questions
  (units follow the locale; the forecast strip ships, capped by
  `forecast_days`; hard expiry hides at 24 h by default).
- `proposal.md`: strike the SQLite bullet's table + schema bump.
- `specs/weather/spec.md`: rewrite the "Reserved provider kind" scenario to
  the observable behaviour — a reserved value warns, resolves to `none`, and
  no fetch occurs (design §6.3 explains why doctor cannot see it from a
  config-loaded value) — and strike the HTTPS-URL-validation clause, since v1
  exposes no configurable provider URL.
- `tasks.md` (the change's own): tick every completed item; delete the DB-table
  tasks (3.1/3.2) with a one-line note that the cache moved to `ui_state`.
- Run `just openspec-validate`.

## Tests

`chrome_tests.rs`:

1. `the_weather_widget_hides_without_a_reading` — `model.weather = None` ⇒
   `masthead_widget("weather", &model)` is `None`.
2. `the_weather_widget_shows_glyph_and_temperature` — a fresh snapshot yields
   text containing the active glyph and `18°C`; with `Sky::Unknown` it is the
   temperature alone with no leading space.
3. `a_stale_reading_dims_and_an_expired_one_disappears` — three snapshots
   across the two thresholds.
4. `the_weather_widget_is_ascii_on_an_ascii_terminal` — via
   `caps::test_override::with_unicode(UnicodeLevel::Ascii, …)`; assert the
   text `is_ascii()` **except** the `°`, or use the degree-stripped
   comparison the `temp` widget test already uses. Match whatever that test
   does rather than inventing a second convention.
5. `weather_sheds_right_after_date` — extend the existing
   `fit_stats_cluster` test: at a width that drops one widget, `date` goes
   first; at the next width down, `weather` goes.

`detail_tests.rs` (or a new `detail/calendar` test module):

6. `the_popup_has_no_weather_block_without_a_reading` — `sections_of` for a
   `CalState` with `weather: None` is byte-identical to today's output. This
   is the guard that keeps every existing e2e baseline valid.
7. `the_weather_block_renders_above_the_world_clocks` — assert the section
   order.
8. `an_expired_reading_suppresses_the_block`.
9. `the_forecast_strip_respects_show_forecast_and_forecast_days`.
10. `the_agenda_hit_test_still_finds_the_agenda` — with the weather block
    present, a click on an agenda row still resolves to
    `CalHit::AgendaRow(..)`. This is the specific regression the new
    `Section::Table` could cause.

## Done criteria

- `just quick thegn-host` clean.
- `cargo nextest run -p thegn-host chrome`,
  `cargo nextest run -p thegn-host detail`, and the help ratchet tests
  (`cargo nextest run -p thegn-host help`) green.
- No help-ratchet allowlist file is modified.
- `just openspec-validate` green.
- With weather disabled, `sections_of` output and the default popup width are
  **unchanged** — so `just e2e` needs no re-record.
- Then, once, as the change's final gate:
  `THEGN_ALLOW_HEAVY=1 just test` and `THEGN_ALLOW_HEAVY=1 just coverage`.
