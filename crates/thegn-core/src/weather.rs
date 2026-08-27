//! Weather domain: the snapshot model, the wttr.in `j1` decode, unit and
//! locale resolution, staleness math, the cache key, and the condition→glyph
//! mapping.
//!
//! **Everything here is pure.** No I/O, no network, no DB, and no clock: every
//! entry point that needs the current time takes `now` (unix seconds) as a
//! parameter, exactly as [`crate::calendar`] does — that module's founding rule
//! is what lets this one be exhaustively unit-tested under the 95% core
//! coverage gate, and what keeps `thegn-core` substrate-free.
//!
//! The provider that actually speaks HTTP lives in `thegn_svc::weather`; the
//! only thing this module knows about wttr.in is the *shape* of a `?format=j1`
//! body it is handed ([`decode_wttr_j1`]).

use serde::{Deserialize, Serialize};

/// The condition class a provider's code collapses to — the granularity the
/// chrome can actually draw (one glyph, see [`sky_glyph`]).
///
/// `Wind` has no WWO code; it exists for the reserved providers (Open-Meteo
/// reports it) and still resolves to a glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sky {
    #[default]
    Unknown,
    Clear,
    Partly,
    Cloudy,
    Fog,
    Rain,
    Snow,
    Storm,
    Wind,
}

impl Sky {
    /// Every class, so a caller (and the glyph test) can iterate the vocabulary.
    pub const ALL: &'static [Sky] = &[
        Sky::Unknown,
        Sky::Clear,
        Sky::Partly,
        Sky::Cloudy,
        Sky::Fog,
        Sky::Rain,
        Sky::Snow,
        Sky::Storm,
        Sky::Wind,
    ];
}

/// The measurement system a snapshot is expressed in. The provider is asked for
/// the right fields; nothing here converts between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Units {
    Metric,
    Imperial,
}

impl Units {
    /// The stable lowercase token used in the cache key (and in config).
    pub fn as_str(self) -> &'static str {
        match self {
            Units::Metric => "metric",
            Units::Imperial => "imperial",
        }
    }
}

/// How old a cached snapshot is, relative to the configured thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
    Expired,
}

/// One forecast day. `hi`/`lo` are already in the snapshot's [`Units`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastDay {
    pub date: chrono::NaiveDate,
    pub hi: f32,
    pub lo: f32,
    pub sky: Sky,
}

/// One reading, as stored in the `ui_state` cache and rendered by the masthead
/// widget and the calendar popup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherSnapshot {
    /// Provider kind token, e.g. `"wttr_in"`.
    pub provider: String,
    /// Provider-reported display name; may be empty.
    pub place: String,
    pub sky: Sky,
    /// Human-readable condition, e.g. `"Partly cloudy"`.
    pub description: String,
    /// Already expressed in `units` — nothing downstream converts.
    pub temp: f32,
    pub feels_like: f32,
    pub hi: f32,
    pub lo: f32,
    pub humidity_pct: u8,
    /// km/h (`Metric`) or mph (`Imperial`).
    pub wind: f32,
    pub units: Units,
    /// Unix **seconds** — the unit [`crate::util::now`] returns, NOT
    /// milliseconds. The mismatch is a classic bug: a millisecond value here
    /// makes every snapshot look like it arrived from the far future, so
    /// [`freshness`] would report `Fresh` forever.
    pub fetched_at: i64,
    /// Empty on an older cached row, hence `#[serde(default)]`.
    #[serde(default)]
    pub forecast: Vec<ForecastDay>,
}

/// A `j1` body that could not be understood. The message deliberately never
/// embeds the body — it can carry the user's location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// WWO condition code → glyph class. Total; unknown codes ⇒ [`Sky::Unknown`].
///
/// Grouped by class rather than enumerated one-for-one: the chrome draws eight
/// glyphs, so "light rain shower" and "moderate rain at times" are both `Rain`.
pub fn sky_from_wwo_code(code: u16) -> Sky {
    match code {
        113 => Sky::Clear,
        116 => Sky::Partly,
        119 | 122 => Sky::Cloudy,
        143 | 248 | 260 => Sky::Fog,
        176 | 263 | 266 | 281 | 284 | 293 | 296 | 299 | 302 | 305 | 308 | 311 | 314 | 317 | 350
        | 353 | 356 | 359 | 362 | 365 | 374 | 377 => Sky::Rain,
        179 | 182 | 185 | 227 | 230 | 320 | 323 | 326 | 329 | 332 | 335 | 338 | 368 | 371 => {
            Sky::Snow
        }
        200 | 386 | 389 | 392 | 395 => Sky::Storm,
        _ => Sky::Unknown,
    }
}

/// The number of forecast days a decode will ever return; the caller slices
/// further to fit its surface.
const MAX_FORECAST_DAYS: usize = 5;

/// Pure decode of a wttr.in `?format=j1` body. `fetched_at` is passed in (unix
/// seconds) because this module never reads a clock.
///
/// Parsed as an untyped [`serde_json::Value`] on purpose: the payload's numbers
/// are JSON *strings* (`"temp_C": "18"`), and fields come and go between
/// deployments — a typed struct fails on every real payload. Missing optional
/// numbers decode as `0.0`. Only two things are fatal: unparseable JSON, and a
/// missing or empty `current_condition`.
pub fn decode_wttr_j1(
    body: &str,
    units: Units,
    fetched_at: i64,
) -> Result<WeatherSnapshot, DecodeError> {
    let root: serde_json::Value = serde_json::from_str(body)
        // Deliberately not `{e}`: serde_json quotes the offending input.
        .map_err(|_| DecodeError("weather: response was not JSON".into()))?;

    let current = root
        .get("current_condition")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| DecodeError("weather: response had no current_condition".into()))?;

    // wttr.in supplies BOTH unit families; we select, never convert.
    let (temp_key, feels_key, wind_key) = match units {
        Units::Metric => ("temp_C", "FeelsLikeC", "windspeedKmph"),
        Units::Imperial => ("temp_F", "FeelsLikeF", "windspeedMiles"),
    };
    let (max_key, min_key) = match units {
        Units::Metric => ("maxtempC", "mintempC"),
        Units::Imperial => ("maxtempF", "mintempF"),
    };

    let days = root.get("weather").and_then(|v| v.as_array());
    let today = days.and_then(|a| a.first());

    let forecast = days
        .map(|a| {
            a.iter()
                .take(MAX_FORECAST_DAYS)
                .filter_map(|d| forecast_day(d, max_key, min_key))
                .collect()
        })
        .unwrap_or_default();

    Ok(WeatherSnapshot {
        provider: "wttr_in".into(),
        place: first_value(
            root.get("nearest_area")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first()),
            "areaName",
        ),
        sky: sky_from_wwo_code(number(current, "weatherCode") as u16),
        description: first_value(Some(current), "weatherDesc"),
        temp: number(current, temp_key),
        feels_like: number(current, feels_key),
        hi: today.map(|d| number(d, max_key)).unwrap_or(0.0),
        lo: today.map(|d| number(d, min_key)).unwrap_or(0.0),
        humidity_pct: number(current, "humidity").clamp(0.0, 100.0) as u8,
        wind: number(current, wind_key),
        units,
        fetched_at,
        forecast,
    })
}

/// One `weather[]` entry → a [`ForecastDay`]. A day with no parseable `date`
/// is dropped: a forecast row with no date has nothing to render against.
fn forecast_day(day: &serde_json::Value, max_key: &str, min_key: &str) -> Option<ForecastDay> {
    let date = chrono::NaiveDate::parse_from_str(day.get("date")?.as_str()?, "%Y-%m-%d").ok()?;
    Some(ForecastDay {
        date,
        hi: number(day, max_key),
        lo: number(day, min_key),
        sky: day_sky(day),
    })
}

/// The class for a forecast day: the 12:00 slot of the 3-hourly `hourly` array
/// (index 4), falling back to the first entry, falling back to `Unknown`.
fn day_sky(day: &serde_json::Value) -> Sky {
    let Some(hourly) = day.get("hourly").and_then(|v| v.as_array()) else {
        return Sky::Unknown;
    };
    let Some(slot) = hourly.get(4).or_else(|| hourly.first()) else {
        return Sky::Unknown;
    };
    sky_from_wwo_code(number(slot, "weatherCode") as u16)
}

/// A `j1` number: a JSON string in practice, a real number if a deployment ever
/// tidies up. Absent or unparseable ⇒ `0.0`, never a failed decode.
fn number(v: &serde_json::Value, key: &str) -> f32 {
    let Some(field) = v.get(key) else {
        return 0.0;
    };
    if let Some(s) = field.as_str() {
        return s.trim().parse::<f32>().unwrap_or(0.0);
    }
    field.as_f64().unwrap_or(0.0) as f32
}

/// The `j1` "array of one object with a `value`" idiom: `obj[key][0].value`,
/// as in `nearest_area[0].areaName[0].value` and `weatherDesc[0].value`. Empty
/// when any hop is absent, and always passed through [`safe_text`] — these are
/// the only two provider-supplied *strings* a snapshot carries.
fn first_value(obj: Option<&serde_json::Value>, key: &str) -> String {
    safe_text(
        obj.and_then(|p| p.get(key))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|e| e.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    )
}

/// The character budget a provider-supplied display string is bounded to.
///
/// Generous against the real payload — wttr.in's longest `weatherDesc` is
/// "Moderate or heavy snow in area with thunder" (43) — and small enough that
/// the popup cannot be sized off a hostile body. The masthead widget never
/// draws either string; the popup truncates to its column width on top of this.
const MAX_TEXT_CHARS: usize = 64;

/// Provider-supplied display text, made safe to draw.
///
/// This is remote data from a keyless third-party service on its way into a
/// terminal compositor, so two properties are established here — at the one
/// seam it enters the domain model through, rather than at each draw site:
///
/// * **No control characters.** `\r` and `\n` are not inert: termwiz acts on
///   them inside a `Change::Text`, resetting the column / advancing the row, so
///   the remainder of the string paints OUTSIDE the popup's clip rect (verified:
///   a `\r` in `place` puts the tail at column 0 of the underlying chrome), and
///   from the last row it scrolls the whole composed frame. ESC and BEL happen
///   to be nerfed to a space by termwiz's cell constructor, but `\r`/`\n` are
///   handled before that, so filtering is what actually closes this.
/// * **Bounded length.** The calendar popup sizes its columns from its widest
///   cell (`render::weather_cols`), and the body limit upstream is 1 MiB.
///
/// Dropped rather than replaced, matching [`cache_key`]'s filter; the result is
/// re-trimmed because stripping can expose new edge whitespace.
fn safe_text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Age classification. `stale_after`/`hard_expiry` are seconds; a `hard_expiry`
/// of 0 disables expiry, and `Expired` wins over `Stale`.
///
/// A `fetched_at` in the future — clock skew, a resumed laptop, an NTP step —
/// is treated as `Fresh` rather than producing a negative age.
pub fn freshness(fetched_at: i64, now: i64, stale_after: u64, hard_expiry: u64) -> Freshness {
    let age = now.saturating_sub(fetched_at);
    if age <= 0 {
        return Freshness::Fresh;
    }
    let age = age as u64;
    if hard_expiry > 0 && age >= hard_expiry {
        return Freshness::Expired;
    }
    if age >= stale_after {
        return Freshness::Stale;
    }
    Freshness::Fresh
}

/// `Some(units)` wins; `None` (= `auto`) resolves from the locale string
/// (`LC_MEASUREMENT` / `LC_ALL` / `LANG`, read by the *caller* — this module
/// never touches the environment).
///
/// The three regions that still measure in Fahrenheit are the US, Liberia and
/// Myanmar; everything else, and an absent or region-less locale, is metric.
pub fn resolve_units(pref: Option<Units>, locale: Option<&str>) -> Units {
    if let Some(u) = pref {
        return u;
    }
    match locale_region(locale).as_deref() {
        Some("US" | "LR" | "MM") => Units::Imperial,
        _ => Units::Metric,
    }
}

/// Pull the region out of a POSIX locale string: `en_US.UTF-8` → `US`.
///
/// Same shape as [`crate::calendar::locale`]'s resolver: handles the `_`/`-`
/// separator and the `.CHARSET` / `@modifier` suffixes; `C` and `POSIX` have no
/// region.
fn locale_region(locale: Option<&str>) -> Option<String> {
    let raw = locale?.trim();
    if raw.is_empty() || raw == "C" || raw == "POSIX" {
        return None;
    }
    let head = raw.split(['.', '@']).next()?;
    let region = head.split(['_', '-']).nth(1)?;
    if region.is_empty() {
        return None;
    }
    Some(region.to_ascii_uppercase())
}

/// The `ui_state` cache key for one configuration:
/// `"<provider>|<location>|<units>"`, the location trimmed and lowercased so
/// `"Berlin"` and `" berlin "` share a row.
///
/// Control characters are dropped: this is a DB key, and a stray newline in a
/// hand-edited `[weather] location` must not split it.
pub fn cache_key(provider: &str, location: &str, units: Units) -> String {
    let loc: String = location
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    format!("{provider}|{loc}|{}", units.as_str())
}

/// `18°C` / `64°F`, rounded to the nearest degree.
///
/// `°` (U+00B0) is plain text, not a caps glyph — Latin-1, width 1, and the
/// existing `temp` masthead widget already writes it directly.
pub fn fmt_temp(t: f32, units: Units) -> String {
    let unit = match units {
        Units::Metric => 'C',
        Units::Imperial => 'F',
    };
    format!("{}\u{00b0}{unit}", t.round() as i64)
}

/// `"12 km/h"` / `"7 mph"`, rounded.
pub fn fmt_wind(w: f32, units: Units) -> String {
    let unit = match units {
        Units::Metric => "km/h",
        Units::Imperial => "mph",
    };
    format!("{} {unit}", w.round() as i64)
}

/// `"just now"` / `"5m ago"` / `"3h ago"` / `"2d ago"` — the staleness note the
/// popup row shows. Never negative: a future `fetched_at` reads as `"just now"`.
pub fn fmt_age(fetched_at: i64, now: i64) -> String {
    let age = now.saturating_sub(fetched_at).max(0);
    if age < 60 {
        "just now".to_string()
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else if age < 86_400 {
        format!("{}h ago", age / 3600)
    } else {
        format!("{}d ago", age / 86_400)
    }
}

/// The condition glyph for the ACTIVE glyph set. Takes the set rather than
/// reaching for it, so this stays pure and no glyph literal escapes the
/// `caps::active_glyphs()` chokepoint.
///
/// [`Sky::Unknown`] renders as `""` — the caller shows the temperature alone
/// rather than a placeholder that means nothing.
pub fn sky_glyph(sky: Sky, set: &crate::termcaps::GlyphSet) -> &'static str {
    match sky {
        Sky::Unknown => "",
        Sky::Clear => set.wx_clear,
        Sky::Partly => set.wx_partly,
        Sky::Cloudy => set.wx_cloudy,
        Sky::Fog => set.wx_fog,
        Sky::Rain => set.wx_rain,
        Sky::Snow => set.wx_snow,
        Sky::Storm => set.wx_storm,
        Sky::Wind => set.wx_wind,
    }
}

#[cfg(test)]
#[path = "weather_tests.rs"]
mod tests;
