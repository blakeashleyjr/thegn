# Chunk 1 — Pure weather domain + condition glyphs (`thegn-core`)

THE-46. Read `.thegn/pipeline/architect/design.md` §2, §4.1, §6.8, §7 first.
Also read `CLAUDE.md` (dev-loop policy: iterate with `just quick thegn-core`,
never per-edit full gates).

## Scope

The whole pure domain: the snapshot model, the wttr.in `j1` decode, unit and
locale resolution, staleness math, the cache key, and the eight condition
glyphs with their ASCII fallbacks. **No config, no HTTP, no DB, no host code.**

`thegn-core` is substrate-free and gated at 95% lines — every function here
must be unit-tested, and none of them may call `Utc::now()`/`Local::now()`
(`now` is always a parameter, the rule `crates/thegn-core/src/calendar/mod.rs`
states in its module doc).

## Files

| File                                     | Action                                                                                  |
| ---------------------------------------- | --------------------------------------------------------------------------------------- |
| `crates/thegn-core/src/weather.rs`       | new                                                                                     |
| `crates/thegn-core/src/weather_tests.rs` | new (`#[path]`-included, the `calendar`/`config_calendar` convention)                   |
| `crates/thegn-core/src/termcaps.rs`      | edit — 8 `GlyphSet` fields, 8 `Glyph` tokens, 8 `resolve` arms, `Glyph::ALL`, two tests |
| `crates/thegn-core/src/lib.rs`           | edit — `pub mod weather;` (one line)                                                    |
| `crates/thegn-core/tests/*`              | none                                                                                    |

**Shared file:** `lib.rs` is also touched by chunk 2 (`pub mod config_weather;`).
Alphabetical ordering puts them apart; add only your own line.

## Approach

### 1. `weather.rs` — the model

Implement exactly the types and signatures in design §4.1. Notes:

- `WeatherSnapshot` derives `Serialize + Deserialize` — it is what gets stored
  as the `ui_state` cache value (chunk 4). Use `#[serde(default)]` on
  `forecast` so an older cached row still loads.
- `fetched_at` is **unix seconds** (`thegn_core::util::now()`'s unit), not
  milliseconds. Say so in the field doc; the mismatch is a classic bug.
- Module doc must state the purity contract (no clock, no I/O) and point at
  `calendar/mod.rs` as the precedent.

### 2. `sky_from_wwo_code` — the condition mapping

wttr.in reports WWO condition codes. Map them by class; the match must be
total with `_ => Sky::Unknown`. Group them like this (verify against the WWO
code list; these are the codes wttr.in actually emits):

| `Sky`    | codes                                                                                                        |
| -------- | ------------------------------------------------------------------------------------------------------------ |
| `Clear`  | 113                                                                                                          |
| `Partly` | 116                                                                                                          |
| `Cloudy` | 119, 122                                                                                                     |
| `Fog`    | 143, 248, 260                                                                                                |
| `Rain`   | 176, 263, 266, 281, 284, 293, 296, 299, 302, 305, 308, 311, 314, 317, 350, 353, 356, 359, 362, 365, 374, 377 |
| `Snow`   | 179, 182, 185, 227, 230, 320, 323, 326, 329, 332, 335, 338, 368, 371                                         |
| `Storm`  | 200, 386, 389, 392, 395                                                                                      |

`Wind` has no WWO code — it exists as a class for the reserved providers
(Open-Meteo does report it) and must still resolve to a glyph.

### 3. `decode_wttr_j1` — the pure decode

Parse with `serde_json::Value`, **not** a typed struct. The payload's numbers
are JSON _strings_ (`"temp_C": "18"`), fields come and go between deployments,
and a typed struct fails on every real payload. Shape:

```json
{
  "current_condition": [{ "temp_C":"18", "temp_F":"64",
                          "FeelsLikeC":"17", "FeelsLikeF":"63",
                          "humidity":"52", "weatherCode":"116",
                          "weatherDesc":[{"value":"Partly cloudy"}],
                          "windspeedKmph":"11", "windspeedMiles":"7" }],
  "nearest_area":       [{ "areaName":[{"value":"Berlin"}],
                           "country": [{"value":"Germany"}] }],
  "weather":            [{ "date":"2026-08-26",
                           "maxtempC":"22","maxtempF":"71",
                           "mintempC":"13","mintempF":"55",
                           "hourly":[ {"weatherCode":"116"}, … ] }]
}
```

Rules:

- `Units::Metric` selects the `*_C` / `*Kmph` fields; `Units::Imperial` the
  `*_F` / `*Miles` ones. **No arithmetic conversion** — wttr.in supplies both.
- `hi`/`lo` come from `weather[0]`'s max/min for the selected unit.
- Per-forecast-day `sky`: `hourly.get(4)` (the 12:00 slot of the 3-hourly
  array), falling back to `hourly.first()`, falling back to `Sky::Unknown`.
- `place`: `nearest_area[0].areaName[0].value`, empty when absent.
- `description`: `weatherDesc[0].value`, trimmed.
- Missing optional numbers default to `0.0` and must not fail the decode.
- Only two things are fatal: unparseable JSON, and a missing/empty
  `current_condition` — both ⇒ `Err(DecodeError(..))` with a short message
  that **does not include the body** (it may embed the location).
- Cap `forecast` at 5 days regardless of what the payload holds; the caller
  slices further.

### 4. Small pure helpers

- `freshness(fetched_at, now, stale_after, hard_expiry)` — `Expired` wins over
  `Stale`; `hard_expiry == 0` disables expiry; `fetched_at > now` (clock skew,
  a resumed laptop) is `Fresh`, never a negative-age panic.
- `resolve_units(pref, locale)` — `Some` wins. For `None`, uppercase the
  locale and treat a region of `US`, `LR` or `MM` as `Imperial`, everything
  else (including `None`) as `Metric`. Match on the region token between `_`
  and `.`/`@` (`en_US.UTF-8` ⇒ `US`). Mirror the shape of
  `crates/thegn-core/src/calendar/locale.rs`.
- `cache_key(provider, location, units)` — `"<provider>|<lowercased, trimmed
location>|<units>"`; an empty location yields `"<provider>||<units>"`. Must
  be stable and contain no newline (it is a DB key).
- `fmt_temp` — round to nearest integer: `18°C` / `64°F`. `°` is `\u{00b0}`,
  written directly (precedent: the `temp` masthead widget in `chrome.rs`).
- `fmt_wind` — `"12 km/h"` / `"7 mph"`, rounded.
- `fmt_age(fetched_at, now)` — `"just now"` under 60 s, then `"5m ago"`,
  `"3h ago"`, `"2d ago"`. Never negative.
- `sky_glyph(sky, set)` — a total match returning the `GlyphSet` field.
  `Sky::Unknown` returns `""` (the caller renders temperature alone).

### 5. `termcaps.rs` — the glyphs

Add eight fields to `GlyphSet`, in both `UNICODE` and `ASCII`, using the table
in design §6.8. Put them together under a short comment block explaining the
class → glyph idea and repeating the BMP/width-1 policy.

```rust
    // Weather condition classes (`crate::weather::Sky`). Same BMP, width-1
    // policy as the rest of the chrome: the obvious picks (⛅ U+26C5, ⚡ U+26A1)
    // are Emoji-Presentation and therefore width 2 — do not reach for them.
    pub wx_clear: &'static str,   // ☀
    pub wx_partly: &'static str,  // ☼
    pub wx_cloudy: &'static str,  // ☁
    pub wx_fog: &'static str,     // ≈
    pub wx_rain: &'static str,    // ☂
    pub wx_snow: &'static str,    // ☃
    pub wx_storm: &'static str,   // ☇
    pub wx_wind: &'static str,    // ↝
```

Then, in the same file:

- add the eight matching `Glyph` variants (`WxClear`, `WxPartly`, `WxCloudy`,
  `WxFog`, `WxRain`, `WxSnow`, `WxStorm`, `WxWind`);
- add them to `Glyph::ALL`;
- add the eight `Glyph::resolve` arms;
- **bump the pinned token count from 47 to 55** in the test near line 1351;
- **add all eight to `unicode_glyphs_are_bmp_and_single_width`'s list.**

If a glyph fails that width assertion, pick a different BMP width-1 character
— never relax the test.

## Tests (`weather_tests.rs`)

Table-driven, and enough to clear the 95 % line gate on `weather.rs`:

1. `wwo_codes_map_to_the_right_class` — one representative code per class plus
   an unknown code ⇒ `Sky::Unknown`.
2. `j1_decodes_metric_and_imperial_from_one_payload` — a captured-shape
   fixture string in the test file; assert `temp`/`wind`/`hi`/`lo` differ
   correctly between `Units::Metric` and `Units::Imperial` and that no
   conversion arithmetic happened (18 vs 64, not 64.4).
3. `j1_numbers_are_strings_and_missing_fields_are_tolerated` — a payload with
   `humidity`/`FeelsLikeC`/`weather` absent still decodes.
4. `j1_rejects_garbage_and_an_empty_current_condition` — both `Err`, and the
   error message does not contain the input body.
5. `forecast_takes_the_midday_hourly_slot` — 8 hourly entries with differing
   codes; assert index 4 wins, and that a 1-entry `hourly` falls back.
6. `freshness_boundaries` — exactly at `stale_after`, exactly at
   `hard_expiry`, `hard_expiry == 0`, and a future `fetched_at`.
7. `units_resolve_from_locale` — `en_US.UTF-8` ⇒ Imperial, `de_DE.UTF-8` ⇒
   Metric, `None` ⇒ Metric, explicit pref beats locale.
8. `cache_key_is_stable_and_case_insensitive` — `"Berlin"` and `" berlin "`
   collide; different units do not.
9. `formatters_read_naturally` — `fmt_temp`, `fmt_wind`, `fmt_age` across the
   boundaries (0 s, 59 s, 60 s, 3 599 s, 24 h, 48 h).
10. `every_sky_class_has_a_glyph_in_both_sets` — iterate every `Sky` variant;
    assert `sky_glyph` is non-empty for all but `Unknown` in `UNICODE` **and**
    `ASCII`, and that the ASCII result `is_ascii()`.

## Done criteria

- `just quick thegn-core` clean (clippy `-D warnings`).
- `cargo nextest run -p thegn-core weather` and
  `cargo nextest run -p thegn-core termcaps` green.
- `Glyph::ALL.len()` pin updated to 55 and the width test lists all eight
  new glyphs.
- No `chrono::Utc::now`, `Local::now`, `std::env`, `reqwest`, `tokio` or
  `rusqlite` reference anywhere in `weather.rs`.
- Nothing outside the files listed above is modified.
