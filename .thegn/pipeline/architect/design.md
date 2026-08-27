# THE-46 — Weather in the date/time surfaces

Architect design for branch `tg/the-46-weather`.
Linear: <https://linear.app/blakeashley/issue/THE-46/weather-integration-into-datetime-module>

Issue body (data): wthrr-the-weathercrab, wttr.in, wego — three reference
implementations. What they tell us is covered in **Provider choice** below.

---

## 1. What already exists (read this before touching anything)

The "date/time module" THE-46 names is real and landed:

| Surface                                       | Where                                                               |
| --------------------------------------------- | ------------------------------------------------------------------- |
| `date` / `clock` masthead widgets             | `crates/thegn-host/src/chrome.rs:1387` (`masthead_widget`)          |
| Widget shedding order                         | `crates/thegn-host/src/chrome.rs:904` (`fit_stats_cluster`)         |
| Calendar popup (grid + agenda + world clocks) | `crates/thegn-host/src/detail/calendar/{mod,render,layout,keys}.rs` |
| Popup dispatch (`"date" \| "clock"`)          | `crates/thegn-host/src/detail.rs:1911`                              |
| `[calendar]` config                           | `crates/thegn-core/src/config_calendar.rs`                          |
| Pure calendar domain                          | `crates/thegn-core/src/calendar/`                                   |
| Off-loop calendar sync                        | `crates/thegn-host/src/hydrate_calendar.rs`                         |
| Calendar provider seam                        | `crates/thegn-svc/src/calendar/`                                    |
| Doctor probes                                 | `crates/thegn-svc/src/seam/registry.rs`                             |

An **OpenSpec change already exists and is tracked**:
`openspec/changes/add-weather-widget/{proposal,design,tasks}.md` +
`specs/weather/spec.md`. That is the behavioural contract this design
implements. Where this design departs from it, §6 says so explicitly and the
reconciliation is assigned work (chunk 5) — the two artifacts must not drift.

There is **no weather code anywhere in the tree today.** This is additive.

---

## 2. Invariants this change must respect

Every one of these is enforced by a gate, not by good intentions.

- **0% idle.** No new timer thread. The refresh rides the existing ticker in
  `hydrate.rs` and emits **no slot at all** when `[weather] enabled = false`
  (the `calendar_every: Option<u64>` pattern). Off ⇒ literally zero wakes.
- **No blocking I/O on the event loop, and none before the first frame.**
  Network _and_ the DB cache read both happen on a `spawn_blocking` task;
  results come back over the refresh channel with a `TerminalWaker` pulse.
- **Render decision stays pure.** A weather snapshot landing sets
  `bars_dirty = true` (`Damage::bars`) — the same channel as the clock tick,
  a two-rect recompose. It must **never** set `dirty` (full chrome). With the
  calendar popup open the overlay path already forces a full frame.
- **`thegn-core` stays substrate-free and 95%-line covered.** All decode,
  unit selection, condition→glyph-class mapping, staleness math and the cache
  key are pure core. No `reqwest`, no tokio, no `Utc::now()` inside them —
  `now` is always a parameter (the `calendar` module's founding rule).
- **Degrade at the edges.** Every condition glyph is a `GlyphSet` field with
  an ASCII fallback, resolved through `caps::active_glyphs()`. No glyph
  literal at a draw site. Chrome glyph policy: **BMP, display-width 1** — the
  `unicode_glyphs_are_bmp_and_single_width` test is the gate, and it rejects
  the obvious picks (`⛅` U+26C5 and `⚡` U+26A1 are Emoji-Presentation, hence
  width 2).
- **Seams, not vendors.** `wttr.in` URLs and `j1` field names live _only_ in
  `crates/thegn-svc/src/weather/wttr_in.rs`. Object-safe trait, `BoxFuture`
  ops (no `async fn` — `test/async-trait-ratchet.txt`), a `Probe` in
  `thegn doctor`, `kind` implemented-or-`reserved`.
- **git/provider is the source of truth; SQLite is a cache.** All cache writes
  are best-effort (`let _ = …` with a `// best-effort:` note).
- **Ignored `Result`s must be deliberate.** `test/ignored-result-ratchet.txt`.
- **Help ratchets.** No new action id, chord, zone or panel context ⇒ no
  ratchet-file edits. Prose updates to `docs/help/bars.md` and
  `docs/help/calendar.md` are still required (the widget id and the popup row
  must be documented where the user looks).
- **e2e.** Weather is network-backed and volatile ⇒ forced **off** under
  `THEGN_E2E=1`, exactly as `[usage]`, `[media]` and `[model_proxy]` are. No
  baseline re-record needed (default-off means default frames are unchanged).

---

## 3. Architecture

```
thegn-core  (pure, no substrate, 95% line gate)
  weather.rs         Sky · Units · Freshness · ForecastDay · WeatherSnapshot
                     decode_wttr_j1()  sky_from_wwo_code()  freshness()
                     resolve_units()   cache_key()   fmt_temp()  sky_glyph()
  termcaps.rs        8 new GlyphSet fields + Glyph tokens (Unicode ⇄ ASCII)
  config_weather.rs  [weather] · WeatherProviderKind · WeatherUnits · validate

thegn-svc   (service seams)
  weather/mod.rs     WeatherProvider (object-safe, BoxFuture) · WeatherError
                     (impl seam::SeamError) · provider_for(cfg, units)
  weather/wttr_in.rs the ONE file that knows wttr.in exists
  seam/registry.rs   weather_probes()  ·  conformance::KNOWN_SEAMS += "weather"

thegn-host  (compositor)
  hydrate_weather.rs cache-read-then-maybe-fetch, off-loop, waker pulse
  hydrate.rs         RefreshKind::{WeatherPoll, Weather} + ticker slot
  run.rs             drain arms · spawn · bars_dirty
  chrome.rs          FrameModel.weather/.weather_cfg · `weather` widget arm
                     · fit_stats_cluster shed order
  detail/calendar/   the popup's WEATHER block, above WORLD CLOCKS
  e2e_freeze.rs      forced off under the freeze
```

Data flow, end to end:

```
ticker (500ms) --WeatherPoll--> run.rs --spawn_blocking--> hydrate_weather
                                                              |
                    ui_state cache read  <---------------------+
                              |                                |
             RefreshKind::Weather (immediate, cache)      provider.fetch()
                              |                                |
                              +---- RefreshKind::Weather <-----+ (best-effort
                              |                                   cache write)
                              v
        run.rs: model.weather = Some(snap); bars_dirty = true
                              |
              +---------------+----------------+
              v                                v
      masthead `weather` widget        calendar popup WEATHER block
```

### Where the cache lives — `ui_state`, not a new table

The OpenSpec design proposed a `weather_cache` table and a `SCHEMA_VERSION`
bump. **Don't.** `ui_state(scope, key, value)` is already a general KV store
used for non-UI state by `account.rs` and `bundle.rs`. One snapshot per
`(provider, location, units)` is exactly one small row.

- scope: `"weather"`
- key: `thegn_core::weather::cache_key(provider, location, units)`
- value: `serde_json::to_string(&WeatherSnapshot)` (the snapshot carries its
  own `fetched_at`, so staleness stays pure)

This removes the migration, the `db_weather.rs` module, the new store trait
**and** the `SCHEMA_VERSION`-collision trap that has repeatedly cost this repo
a merge conflict. Reads/writes go through the existing
`WorkspaceStore::{get_ui_state, set_ui_state}`.

---

## 4. Frozen interfaces

These signatures are the contract between chunks. A chunk may add to its own
module freely; it may **not** change anything below without the change being
propagated to every chunk that names it.

### 4.1 `thegn_core::weather` (chunk 1)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sky { #[default] Unknown, Clear, Partly, Cloudy, Fog, Rain, Snow, Storm, Wind }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Units { Metric, Imperial }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness { Fresh, Stale, Expired }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastDay {
    pub date: chrono::NaiveDate,
    pub hi: f32,
    pub lo: f32,
    pub sky: Sky,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherSnapshot {
    pub provider: String,        // "wttr_in"
    pub place: String,           // provider-reported display name; may be empty
    pub sky: Sky,
    pub description: String,     // "Partly cloudy"
    pub temp: f32,               // already expressed in `units`
    pub feels_like: f32,
    pub hi: f32,
    pub lo: f32,
    pub humidity_pct: u8,
    pub wind: f32,               // km/h (Metric) or mph (Imperial)
    pub units: Units,
    pub fetched_at: i64,         // unix SECONDS (thegn_core::util::now())
    #[serde(default)]
    pub forecast: Vec<ForecastDay>,
}

/// WWO condition code → glyph class. Total; unknown codes ⇒ `Sky::Unknown`.
pub fn sky_from_wwo_code(code: u16) -> Sky;

/// Pure decode of a wttr.in `?format=j1` body. `fetched_at` is passed in.
pub fn decode_wttr_j1(
    body: &str,
    units: Units,
    fetched_at: i64,
) -> Result<WeatherSnapshot, DecodeError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);   // Display + std::error::Error

/// Age classification. `stale_after`/`hard_expiry` in seconds; a `hard_expiry`
/// of 0 disables expiry. A `fetched_at` in the future is treated as Fresh.
pub fn freshness(fetched_at: i64, now: i64, stale_after: u64, hard_expiry: u64) -> Freshness;

/// `Some(units)` wins; `None` (= `auto`) resolves from the locale string
/// (`LC_MEASUREMENT`/`LC_ALL`/`LANG`): US/LR/MM ⇒ Imperial, else Metric.
pub fn resolve_units(pref: Option<Units>, locale: Option<&str>) -> Units;

/// The `ui_state` cache key for one configuration.
pub fn cache_key(provider: &str, location: &str, units: Units) -> String;

/// `18°C` / `64°F`. `°` (U+00B0) is plain text, not a caps glyph — the
/// existing `temp` masthead widget already writes it directly.
pub fn fmt_temp(t: f32, units: Units) -> String;

/// `"12 km/h"` / `"7 mph"`.
pub fn fmt_wind(w: f32, units: Units) -> String;

/// The condition glyph for the ACTIVE glyph set. Takes the set rather than
/// reaching for it, so this stays pure and no literal escapes the chokepoint.
pub fn sky_glyph(sky: Sky, set: &crate::termcaps::GlyphSet) -> &'static str;

/// `"3h ago"` / `"2d ago"` — the staleness note the popup row shows.
pub fn fmt_age(fetched_at: i64, now: i64) -> String;
```

### 4.2 `thegn_core::config_weather` (chunk 2)

```rust
pub const MIN_REFRESH_SECS: u64 = 600;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WeatherConfig {
    pub enabled: bool,                     // false
    pub provider: WeatherProviderKind,     // struct default: WttrIn
    pub location: String,                  // "" ⇒ provider infers from request IP
    pub units: WeatherUnits,               // Auto
    pub refresh_interval_secs: u64,        // 1800, floored at MIN_REFRESH_SECS
    pub stale_after_secs: u64,             // 10800 (3h)
    pub hard_expiry_secs: u64,             // 86400 (24h); 0 disables
    pub show_forecast: bool,               // true — the popup's day strip
    pub forecast_days: usize,              // 3
    pub timeout_secs: u64,                 // 10, clamped 3..=60 in the accessor
    pub api_key: String,                   // "" — SecretRef only, reserved kinds
}

impl WeatherConfig {
    /// Effective refresh interval, floored. Floored HERE, not at the ticker,
    /// so every caller inherits it (the `CalendarAccount::refresh_secs` rule).
    pub fn refresh_secs(&self) -> u64;
    /// `None` when disabled, `provider = "none"`, or the kind is reserved —
    /// in which case the ticker emits no slot at all.
    pub fn poll_secs(&self) -> Option<u64>;
    pub fn units_pref(&self) -> Option<crate::weather::Units>;
    pub fn resolved_units(&self, locale: Option<&str>) -> crate::weather::Units;
    pub fn timeout(&self) -> std::time::Duration;
    /// True when a fetch should even be attempted.
    pub fn is_active(&self) -> bool;
}

config_enum! {
    pub enum WeatherProviderKind : "weather provider" {
        None           = "none",
        WttrIn         = "wttr_in" | "wttr",
        OpenMeteo      = "open_meteo" reserved,
        OpenWeatherMap = "openweathermap" | "owm" reserved,
    } default = None;      // <- see §6.3: enum default None, struct default WttrIn
}

config_enum! {
    pub enum WeatherUnits : "weather units" {
        Auto     = "auto",
        Metric   = "metric" | "si" | "celsius",
        Imperial = "imperial" | "us" | "fahrenheit",
    } default = Auto;
}

pub fn validate_weather(cfg: &WeatherConfig) -> Vec<String>;
```

`Config` gains `pub weather: WeatherConfig` and `config.rs` re-exports the
types alongside the `config_calendar` re-export at `config.rs:3206`.

### 4.3 `thegn_svc::weather` (chunk 3)

```rust
#[derive(Debug, Clone)]
pub enum WeatherError {
    NotConfigured,
    Network(String),
    Api(String),
    Parse(String),
    Unsupported(&'static str),
}
impl std::error::Error for WeatherError {}
impl thegn_core::seam::SeamError for WeatherError {
    fn class(&self) -> ErrorClass;              // Network→Transient, Api→Other,
                                                // NotConfigured→NotConfigured, …
    fn unsupported(op: &'static str) -> Self;
}

pub trait WeatherProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;
    /// Current conditions + a short forecast, in ONE round trip.
    fn fetch<'a>(&'a self) -> BoxFuture<'a, Result<WeatherSnapshot, WeatherError>>;
}

/// The backend for a config, or `None` for disabled / `none` / reserved.
pub fn provider_for(
    cfg: &WeatherConfig,
    units: Units,
) -> Option<Box<dyn WeatherProvider>>;
```

### 4.4 `FrameModel` additions (chunk 4)

```rust
/// Latest weather reading (`[weather]`). `None` while disabled, before the
/// first delivery, or once hard-expired. Loop-owned like `stats` and `usage`:
/// pushed by the weather task, never by hydration, so it survives a model swap.
pub weather: Option<thegn_core::weather::WeatherSnapshot>,
/// `[weather]` mirrored into the model, so the widget and the popup row read
/// thresholds/units without a config handle (the `usage_cfg` precedent).
pub weather_cfg: thegn_core::config_weather::WeatherConfig,
```

### 4.5 Refresh channel (chunk 4)

```rust
/// Time to consider a weather refresh: read the cache, and fetch if stale.
/// Emitted by the ticker on `[weather] refresh_interval_secs` (floored at 600)
/// plus a one-shot slot shortly after launch. No slot at all when disabled.
WeatherPoll,
/// A weather reading — from the cache (immediately, at launch) or from a
/// successful fetch. Boxed to keep `RefreshKind` small.
Weather(Box<thegn_core::weather::WeatherSnapshot>),
```

---

## 5. Provider choice

- **wttr.in — implemented, the only v1 backend.** Keyless. `?format=j1`
  returns current conditions **and** a 3-day forecast in one GET. Location is
  a path segment; omit it and the service infers a city from the request IP.
  It also returns _both_ metric and imperial fields (`temp_C`/`temp_F`,
  `windspeedKmph`/`windspeedMiles`), so unit selection happens in the pure
  decode and **no conversion arithmetic is needed at all**.
  Weakness: a community service with availability wobbles — which the
  last-good cache and quiet-failure posture exist for.
- **Open-Meteo — reserved.** Also keyless (it is what `wthrr` uses), but needs
  a geocoding call before the forecast call: two requests, two things to
  cache. First candidate to graduate if wttr.in reliability disappoints.
- **OpenWeatherMap — reserved.** Keyed. Present in the enum now so the
  credential-custody rule (`api_key` is a SecretRef, never a raw value) is
  fixed before anything depends on it.
- **wego — not a kind.** It is a client, not a service, and its backends need
  keys. It informs the reserved-keyed posture and nothing else.

---

## 6. Decisions, and deltas from the OpenSpec change

These are deliberate departures. Chunk 5 folds them back into
`openspec/changes/add-weather-widget/` so the two do not drift.

### 6.1 Cache in `ui_state`, not a new table (delta)

See §3. Kills the migration, the store trait, and the `SCHEMA_VERSION`
collision. `SCHEMA_VERSION` is **not** bumped by this change.

### 6.2 `spawn_blocking`, not the background lane (delta)

The OpenSpec design says "background lane, `Background` QoS". That is wrong
here, for a reason already written down in `actions.rs` above `spawn_usage`:
`sched::spawn_bg` **silently drops** work when its 8 permits are exhausted, on
the assumption a periodic trigger retries shortly. The lane is busiest during
startup — exactly when the one-shot first poll fires — so the badge would stay
empty until the next full interval (here: 30 minutes). Weather is one
network-bound task every half hour out of 32 blocking threads; it uses
`tokio::task::spawn_blocking` directly, like `spawn_usage`.

### 6.3 Reserved kinds fall back to `none`, not to `wttr_in` (delta)

The OpenSpec spec has a scenario "`provider = "open_meteo"` ⇒ config loads, no
fetch occurs, doctor reports reserved". With `config_enum!` as written, a
reserved value **fails `from_str_validated`, warns, and deserializes to the
enum's `default`** — so with `default = WttrIn` the user would silently get
wttr.in, which is worse than useless (it is unexpected egress).

Fix: `default = None` on the _enum_, `WttrIn` on the _struct_ (`#[serde(default)]`
on the container fills a **missing** key from `WeatherConfig::default()`, while
a **present-but-invalid** key goes through the enum's `Deserialize`). Result:

- key absent ⇒ `wttr_in` (the sane default);
- `provider = "open_meteo"` ⇒ warn + `none` ⇒ `poll_secs() == None` ⇒ no
  fetch, no thread, no widget. Exactly the spec's intent.

The `is_reserved()` arm in `weather_probes` is kept for shape parity with the
other seams (and for a programmatically-constructed config), but note in the
code that config cannot reach it. Chunk 5 rewrites the spec scenario to match
the observable behaviour.

### 6.4 e2e: forced off, not pinned (delta)

The OpenSpec tasks say "pin the widget + popup row in `e2e_freeze.rs`". The
house precedent for a network-backed, live-numbers surface is to **disable**
it under the freeze (`[media]`, `[usage]`, `[model_proxy]` all do). A driven
instance must not reach the network. So: `cfg.weather.enabled = false` in
`e2e_freeze::apply_to_config`, plus a bullet in the module doc. No baselines
change, so no `just e2e-update` run is needed.

### 6.5 No HTTPS validation key (delta)

v1 exposes no user-configurable provider URL — the wttr.in base is a constant
inside the impl file. There is nothing to validate. The rule survives as a
constant (`https://wttr.in/`) and as a code comment; `validate_weather` keeps
only the checks that can actually fire (units, intervals, `api_key` custody,
location length).

### 6.6 No capability-catalog row

Weather has no externally invokable operation — it is chrome fed by a
background task. `CATALOG` is untouched. (A `thegn weather` CLI verb, if ever
wanted, enters the catalog in its own change.)

### 6.7 Placement in the surfaces

- **Masthead:** the widget renders where the user places it in
  `[bars] top_right`; the shipped default gains `"weather"` immediately
  **before** `"date"`. Shed order (`fit_stats_cluster`) becomes
  `["date", "weather", "uptime", …]` — `date` is the softest (the clock
  carries the same information) and weather is next: the user opted in, so it
  should outlive `uptime`/`load`/`freq`, but it is still not `cpu`.
- **Popup:** a `WEATHER · <place>` heading + table **above** `WORLD CLOCKS`.
  Weather is "here, right now"; the clocks are "elsewhere, right now" — that
  ordering reads correctly, and it keeps the clocks anchored at the bottom
  where existing users expect them. Absent entirely (not a blank block) when
  disabled, never fetched, or hard-expired.
- **No new action id, chord, zone or panel context.** Clicking the widget
  opens the existing calendar popup via the existing `open-calendar` path,
  which means the help ratchets need no allowlist edits.

### 6.8 Glyphs

Chrome policy forbids astral-plane and emoji-presentation glyphs (`⛅` U+26C5
and `⚡` U+26A1 are width 2 — that is the U+26C1 bug class the policy exists
for). Nine `Sky` classes, eight glyphs (Unknown renders temperature only):

| Class    | Unicode      | ASCII | Note                                        |
| -------- | ------------ | ----- | ------------------------------------------- |
| `Clear`  | `\u{2600}` ☀ | `*`   | Ambiguous ⇒ width 1                         |
| `Partly` | `\u{263C}` ☼ | `*`   | Ambiguous ⇒ width 1                         |
| `Cloudy` | `\u{2601}` ☁ | `=`   | Ambiguous ⇒ width 1                         |
| `Fog`    | `\u{2248}` ≈ | `~`   | Ambiguous ⇒ width 1                         |
| `Rain`   | `\u{2602}` ☂ | `'`   | Ambiguous ⇒ width 1                         |
| `Snow`   | `\u{2603}` ☃ | `#`   | Ambiguous ⇒ width 1                         |
| `Storm`  | `\u{2607}` ☇ | `!`   | Neutral ⇒ width 1; literally "thunderstorm" |
| `Wind`   | `\u{219D}` ↝ | `~`   | Neutral ⇒ width 1                           |

The table is a _proposal_, not a guarantee: the authority is
`unicode_glyphs_are_bmp_and_single_width`, which chunk 1 extends with all
eight. If a pick fails, swap it — do not weaken the test.

---

## 7. Known traps

1. **wttr.in `j1` numbers are JSON strings.** `"temp_C": "18"`, not `18`.
   Parse with `str::parse::<f32>()` and tolerate absence; a serde struct with
   numeric fields fails on every payload.
2. **`spawn_bg` silently drops work.** §6.2. Do not "tidy" the weather task
   onto it.
3. **`dirty` vs `bars_dirty`.** Setting `dirty` on a weather delivery turns a
   half-hourly datum into a full-chrome repaint. Use `bars_dirty`.
4. **Deliver-only-on-change.** Compare against `model.weather` before setting
   `bars_dirty`, exactly as the usage drain does (`accounts_moved`). A cached
   redelivery of an identical snapshot must not repaint.
5. **Don't log the location.** It is the one piece of user data this feature
   handles. `tracing` events carry the provider and the error class only.
6. **`Glyph::ALL.len()` is pinned at 47** in `termcaps.rs`. Adding eight
   tokens without bumping it fails `just test` with a message that reads like
   something else.
7. **`config_enum!` marked-definition count is pinned at 88** in
   `config_validate.rs`. Two new enums ⇒ 90, with a dated comment line in the
   running history there.
8. **`conformance::KNOWN_SEAMS`** must gain `"weather"` or every registry
   conformance assertion fails the moment a probe is emitted.
9. **`test/env-overlay-ratchet.txt`** — a new config key with no
   `THEGN_<SECTION>_<KEY>` override must be pinned there. Give
   `[weather] enabled` a real knob (`THEGN_WEATHER_ENABLED`, useful for muse
   and for a quick try) and pin the rest with a reason line.
10. **This shell often runs inside a live thegn.** Anything that opens the DB
    in a test must isolate `XDG_STATE_HOME`.
11. **`nix/source.nix` needs no edit** — `crates/`, `config/` and `docs/help/`
    are already whole-directory roots.

---

## 8. Chunk map

Five chunks, file-disjoint. `crates/thegn-core/src/lib.rs` and
`crates/thegn-host/src/chrome.rs` are each touched by two chunks at
non-adjacent anchors; those are called out in the chunk files.

| #   | Title                                         | Crate             | Depends on     |
| --- | --------------------------------------------- | ----------------- | -------------- |
| 1   | Pure weather domain + glyphs                  | thegn-core        | —              |
| 2   | `[weather]` config + validation + example     | thegn-core        | 1 (types only) |
| 3   | Provider seam + doctor probe                  | thegn-svc         | 1, 2           |
| 4   | Host data plane (fetch, cache, ticker, model) | thegn-host        | 1, 2, 3        |
| 5   | Masthead widget, popup block, docs, openspec  | thegn-host + docs | 1, 2, 4        |

**Landing order:** 1 → 2 → 3 → 4 → 5. Chunks 1 and 2 can be written in
parallel (2 codes against §4.1); 3 needs 1+2 compiled; 5 needs 4's
`FrameModel` fields.

---

## 9. Definition of done (whole change)

- `[weather]` absent or `enabled = false`: **no** network request, **no**
  ticker slot, **no** widget, **no** popup block, **no** behaviour change of
  any kind. Verified by a test that a default `Config` yields
  `weather.poll_secs() == None`.
- `enabled = true`: the widget shows a glyph + temperature within seconds of
  launch from cache, refreshes on a floored ≥600 s cadence, dims when stale,
  disappears when hard-expired, and never produces a toast or a status-line
  error on failure.
- `thegn doctor` reports the weather seam.
- Gates, run **once** at the end (dev-loop policy — iterate with
  `just quick <crate>`):
  `just quick thegn-core && just quick thegn-svc && just quick thegn-host`,
  then `THEGN_ALLOW_HEAVY=1 just test`, `THEGN_ALLOW_HEAVY=1 just coverage`,
  `just openspec-validate`. `just e2e` is unaffected (default-off).
