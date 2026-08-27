# Chunk 1 — done: pure weather domain + condition glyphs (`thegn-core`)

THE-46, stage `code`, chunk 1. Branch `tg/the-46-weather`.

## What landed

| File                                     | Action                                                         |
| ---------------------------------------- | -------------------------------------------------------------- |
| `crates/thegn-core/src/weather.rs`       | new — the whole pure domain (~330 lines)                       |
| `crates/thegn-core/src/weather_tests.rs` | new — 12 tests, `#[path]`-included                             |
| `crates/thegn-core/src/termcaps.rs`      | edit — 8 fields ×2 sets, 8 tokens, 8 arms, `ALL`, 3 tests      |
| `crates/thegn-core/src/lib.rs`           | edit — `pub mod weather;` (one line, between `viz` and `work`) |

Nothing else is modified (`git status` confirms exactly these four paths).

## Public surface — exactly as design §4.1 froze it

`Sky` · `Units` · `Freshness` · `ForecastDay` · `WeatherSnapshot` ·
`DecodeError`, and `sky_from_wwo_code` · `decode_wttr_j1` · `freshness` ·
`resolve_units` · `cache_key` · `fmt_temp` · `fmt_wind` · `fmt_age` ·
`sky_glyph`. Later chunks can code against these verbatim.

Two **additions** inside the module (the design permits a chunk to add to its
own module; neither changes a frozen signature):

- `Sky::ALL: &'static [Sky]` — the exhaustive class list, so chunk 5 (and the
  glyph test) can iterate the vocabulary the way `Glyph::ALL` is iterated.
- `Units::as_str(self) -> &'static str` → `"metric"` / `"imperial"`. The cache
  key needed a stable token; chunk 2's `WeatherUnits` config enum can map onto
  it rather than re-inventing the strings.

## Decisions inside the chunk's latitude

- **`decode_wttr_j1` tolerates both string and real-number `j1` fields.** The
  spec's trap #1 is that the numbers are JSON strings; `number()` parses the
  string form first and falls back to `as_f64`, so a future wttr.in deployment
  that tidies the types up does not silently zero every reading.
- **A forecast day with no parseable `date` is dropped**, not defaulted —
  `ForecastDay.date` is non-optional and a dateless row has nothing to render
  against. Tested.
- **`humidity_pct` is clamped to 0..=100** before the `as u8` cast. An
  out-of-range `"250"` would otherwise be a plain truncating cast. Tested.
- **`cache_key` filters control characters** as well as trimming/lowercasing.
  The spec requires "no newline"; a hand-edited `[weather] location` is the
  realistic source of one, so all control chars go rather than just `\n`.
- **Error strings** are `"weather: response was not JSON"` /
  `"weather: response had no current_condition"` — the serde error is
  deliberately discarded because `serde_json` quotes the offending input, which
  is the location leak trap #5 warns about. A test asserts a planted location
  string never reaches the message.
- **Glyph picks are the design's table verbatim** and all eight pass
  `unicode_glyphs_are_bmp_and_single_width` unchanged — no substitution needed.

## termcaps.rs specifics

- `Glyph::ALL.len()` pin: **47 → 55**.
- The eight new fields were added to the width/BMP test list **and** to the
  ASCII-fallback test list (`all_fallback_glyphs_are_ascii`). The chunk spec
  named two tests; adding the third list is the same file and keeps the ASCII
  assertion honest for the new fields.

## Verification

- `cargo nextest run -p thegn-core weather` — **12/12 pass**.
- `cargo nextest run -p thegn-core termcaps` — **35/35 pass**, including
  `unicode_glyphs_are_bmp_and_single_width` and
  `glyph_token_covers_every_glyphset_field` (the 55 pin).
- `cargo clippy -p thegn-core --all-targets` — clean for every file in this
  chunk (lib **and** test targets).
- `nix fmt` applied to all four files.
- `weather.rs` contains no `Utc::now` / `Local::now` / `std::env` / `reqwest` /
  `tokio` / `rusqlite` reference (grep-verified). `now` is always a parameter.

Per the dev-loop policy: `just quick` / targeted nextest only. The full gates
(`just test`, `just coverage`, `just ci`) are the once-at-the-end pre-PR run for
the whole change, not per-chunk.

## One thing for whoever runs the final gate

`just quick thegn-core` currently fails on a **pre-existing** lint that predates
this branch:

```
error: manual implementation of `ok`
  --> crates/thegn-core/src/sandbox_cpucap.rs:297:16
      = note: `-D clippy::manual-ok-err` implied by `-D warnings`
```

`sandbox_cpucap.rs` is untouched here (`git diff --name-only` lists only
`lib.rs` and `termcaps.rs`), and the chunk spec's "nothing outside the listed
files is modified" rule means I left it alone. It is a one-line fix
(`v.parse().ok()`) but it belongs to whoever owns that ratchet/clippy bump, not
to this chunk. **Expect `just quick thegn-core` to be red on it until then** —
the weather and termcaps code itself is clean.

## Handoff notes for chunks 2–5

- Chunk 2 adds `pub mod config_weather;` to `lib.rs`. Alphabetically it sits in
  the `config_*` run near the top, far from my `weather` line — no conflict.
- Chunk 2's `WeatherUnits` should convert to `weather::Units` (and can reuse
  `Units::as_str` for the TOML token).
- Chunk 3's `WeatherError` wraps `DecodeError`, which implements
  `std::error::Error` + `Display`.
- Chunk 4 stores `serde_json::to_string(&WeatherSnapshot)` under
  `cache_key(provider, location, units)` and passes `util::now()` (unix
  **seconds**) as `fetched_at`. `forecast` is `#[serde(default)]`, so an older
  cached row still loads.
- Chunk 5 renders `sky_glyph(snap.sky, caps::active_glyphs())`;
  `Sky::Unknown` returns `""`, so the widget shows temperature alone rather
  than a placeholder.
