# Chunk 5 — done: masthead widget, calendar block, docs, openspec (FINAL)

THE-46, stage `code`, chunk 5. Branch `tg/the-46-weather`, commits
`cfae5e3d` (impl + help), `e6b0baf2` (tests), `a3978c4f` (roadmap + openspec),
`f0e0a4bb` (the pre-existing clippy blocker chunks 1–4 kept detouring around).

## What landed

| File                                              | Action                                                                     |
| ------------------------------------------------- | -------------------------------------------------------------------------- |
| `crates/thegn-host/src/chrome.rs`                 | edit — `"weather"` widget arm, `fit_stats_cluster` victim list + doc       |
| `crates/thegn-host/src/chrome_tests.rs`           | edit — 5 tests + 2 fixtures                                                |
| `crates/thegn-host/src/calendar_docs.rs`          | edit — `WxUiCfg` + `from_config`                                           |
| `crates/thegn-host/src/detail.rs`                 | edit — `calendar::open(ctx, near, model)`, `"weather"` joins the arm       |
| `crates/thegn-host/src/detail/calendar/mod.rs`    | edit — `CalState.weather`/`.wx`, `open`, `preferred_cols`, `retick_open`   |
| `crates/thegn-host/src/detail/calendar/render.rs` | edit — `weather_sections` + `weather_cols`, wired into `sections_of`       |
| `crates/thegn-host/src/detail_tests.rs`           | edit — 5 tests + 4 helpers                                                 |
| `crates/thegn-host/src/sections.rs`               | edit — `table_col_widths` extracted from `draw_table`, `table_cols` added  |
| `crates/thegn-host/src/run.rs`                    | edit — the two `retick_open` call sites gain `model.weather.as_ref()`      |
| `crates/thegn-core/src/config.rs`                 | edit — `"weather"` in `BarsConfig::default().top_right` + the doc list     |
| `crates/thegn-core/src/config_tests_coverage.rs`  | edit — the pinned `top_right` default                                      |
| `crates/thegn-core/src/sandbox_cpucap.rs`         | edit — the `manual_ok_err` `#[allow]` (see below)                          |
| `config/config.toml.example`                      | edit — `top_right` + the widget-list comment                               |
| `docs/help/bars.md`, `docs/help/calendar.md`      | edit — widget prose, the four-block sentence, the box diagram, `[weather]` |
| `tasks.md`                                        | edit — row 760 under group AM                                              |
| `openspec/changes/add-weather-widget/*`           | edit — design/proposal/tasks/spec reconciled with what shipped             |

## Done criteria

- `just quick thegn-host` — **clean**, and for the first time in this change it
  is clean with no local detour (see the clippy note below).
- `cargo clippy -p thegn-host -p thegn-core --all-targets -- -D warnings` — clean.
- `cargo nextest run -p thegn-host weather` — **13/13**;
  `… -E 'test(detail::) or test(chrome::) or test(sections)'` — **206/206**;
  `… -E 'test(help) or test(ratchet)'` — **77/77**.
- **No help-ratchet allowlist file is modified.** No new action id, chord, zone
  or panel context, so there was nothing to pin. `git status test/` is empty and
  the five shell ratchets re-run clean at their existing counts (forge-leak 4,
  async-trait 0, ignored-result 323, json-emit 14, element 3).
- `just openspec-validate` — **165 passed, 0 failed**.
- **Weather off ⇒ nothing changes.** `the_popup_has_no_weather_block_without_a_reading`
  asserts the popup's section list is exactly `["WORLD CLOCKS"]` and its width is
  still 44 columns; `the_weather_widget_hides_without_a_reading` asserts the
  widget occupies nothing. So `just e2e` needs no re-record.
- **Final gates, run once:** `THEGN_ALLOW_HEAVY=1 just test` — **6464/6464 pass,
  20 skipped**. `THEGN_ALLOW_HEAVY=1 just coverage` — **`coverage: core ≥95%
lines`**.

## Decisions inside the chunk's latitude

- **`retick_open` gained a `weather` parameter, and run.rs's two call sites with
  it.** Chunk 4's handoff said the popup would pick a new reading up "with no
  further plumbing" — it can't: `retick_open(slot)` had no way to see the model,
  so an open popup would have kept its open-time snapshot while the bar moved on.
  That directly contradicts the specced `CalState.weather` doc ("Refreshed by
  `retick_open` from the same model the widget reads, so the block never
  disagrees with the masthead"), so the parameter is the smaller change. Two
  lines in `run.rs` (a file outside this chunk's list, but chunk 4 is landed and
  I am the only coder, so there was nothing to conflict with).
- **`table_cols` was extracted into `sections.rs` rather than reimplemented in
  `render.rs`.** `preferred_cols` has to know how wide the weather table will be,
  and `Cell::width` is private to `sections`. Copying `draw_table`'s sizing loop
  into the calendar would have created two rules that must agree forever; instead
  `draw_table` and the new `table_cols` now share `table_col_widths`, so the
  popup measures the block exactly the way it will be drawn. `draw_table` is
  otherwise unchanged.
- **The forecast rows are 3 cells against the conditions row's 6**, as specced.
  `draw_table` sizes to `max(ncol)` so the weekday sits under the description
  column and the day glyph under the temperature. It is a little gappy and it is
  what the spec asked for; the alternative (a separate table) would have added a
  second `Section::Table` to the popup for no gain.
- **`"weather"` joined the `"date" | "clock"` arm of `widget_detail_inner`.** Not
  in the chunk's file list, but design §6.7 says clicking the widget opens the
  existing calendar popup and both help pages now say so; without the arm the
  click would have done nothing. `MonitorTab::for_widget("weather")` is `None`,
  like `date`/`clock`, so the monitor-tab drift test is unaffected.
- **`config.toml.example` and `config_tests_coverage.rs` were updated with the
  default.** The chunk listed only `config.rs`, but `bars_config_defaults` pins
  the `top_right` vector and `example_config_documents_every_section_and_key`
  reads the example — a default that changes in one place and not the others is
  a red suite, not a smaller diff.
- **`WxUiCfg` lives in `calendar_docs.rs` beside `CalUiCfg`**, the spec's first
  suggested home. `Copy` (four scalars), so the popup snapshot costs nothing.
- **The heading note is `None` when fresh** rather than an empty string: a
  `Section::Heading`'s note is right-aligned ghost text, and an empty one would
  still reserve nothing but reads wrong in the section list a test compares.

## The pre-existing clippy blocker is now fixed

`clippy::manual_ok_err` at `crates/thegn-core/src/sandbox_cpucap.rs:297` blocked
`just quick <crate>` for chunks 1–4, each of which applied a local fix, verified,
and reverted it. It is fixed here (`f0e0a4bb`), taking chunk 4's recommendation:
an `#[allow]` with the reason, **not** clippy's own suggestion. Clippy suggests
`return v.parse().ok();`, and `sandbox_cpucap.rs` is not in
`test/ignored-result-ratchet.txt` — so the literal fix would have traded a clippy
error for a new ratchet violation. It is not an ignored `Result` in any real
sense (the `None` is the answer, not a swallowed error), which is exactly what
the `#[allow]`'s comment says.

## Notes for the reviewer

- **`hit()` was not changed, deliberately.** The agenda is detected as "the first
  `Section::Table` after the grid"; the weather table is pushed _after_ the
  agenda's and _before_ the clocks', so the rule still holds. Its comment now
  says so explicitly and `the_agenda_hit_test_still_finds_the_agenda` walks the
  real section stack and clicks the agenda's first row with the weather block
  present. Anything new that draws a table into this popup must go after the
  agenda or fix `hit()` properly.
- **`preferred_cols` widens by `max(weather_cols)` only**, and `weather_cols` is
  `0` for absent/expired. `an_expired_reading_suppresses_the_block` asserts the
  popup drops back to 44 columns when the reading ages out, so a long-offline
  machine doesn't leave a wider-than-usual popup behind.
- **e2e was not re-recorded and should not need to be.** Weather is forced off
  under the freeze (chunk 4), and the two "unchanged when off" tests are the
  guard. I did not run `just e2e` — it is a known-broken/stale gate per the repo
  notes, and nothing in this change alters a default-configuration frame.
- **The openspec reconcile folded in five deltas**, each labelled
  `(implemented delta)` in `design.md`: the `ui_state` cache (no table, no
  `SCHEMA_VERSION` bump), `spawn_blocking` over `spawn_bg`, reserved kinds
  falling back to `none`, e2e forced off rather than pinned, and no HTTPS-URL
  validation key. The three open questions are resolved in place under
  "Resolved questions"; `proposal.md`'s SQLite bullet and the spec's
  reserved-kind scenario, HTTPS clause, background-lane clause and hard-expiry
  scenario were rewritten to the observable behaviour. The change's own
  `tasks.md` ticks everything except 5.5, which is the gate run recorded above —
  tick it when the change is archived.
