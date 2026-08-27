# Design — weather in the date/time surfaces

## Layering

```
thegn-core (pure, 95% gate)
  weather.rs          — WeatherSnapshot/Forecast model, units conversion,
                        condition → glyph-class mapping, staleness math,
                        wttr.in j1 decode (pure JSON → model)
  config_weather.rs   — [weather], WeatherProviderKind config_enum!

thegn-svc (service seams)
  weather/mod.rs      — object-safe WeatherProvider (BoxFuture ops), resolve()
  weather/wttr_in.rs  — the vendor impl: URL building, ?format=j1 fetch
  seam/registry.rs    — weather probe (implemented / unreachable / reserved)

thegn-host (compositor)
  hydrate_weather.rs  — spawn_blocking pass: cache-read at start, fetch on
                        interval, channel + waker delivery
  bars: `weather` widget · calendar popup WEATHER block · e2e_freeze off
```

Everything decidable is pure core (decode, units, glyph class, staleness), so
the coverage gate does the heavy testing; the svc impl is a thin HTTP shell
smoke-covered via the seam probe; the host owns rendering and lane wiring.

## Decisions

### wttr.in first, Open-Meteo reserved

- **wttr.in (implemented default):** keyless, one GET, `?format=j1` returns
  current conditions + a 3-day forecast in one payload; location is a
  free-form string appended to the path, or omitted for server-side IP
  inference. Weaknesses: it is a community service with occasional
  availability wobbles — which the last-good cache + quiet-failure posture is
  designed around (its own docs recommend exactly this for status-bar use).
- **`open_meteo` (reserved):** also keyless (wthrr is built on it) but needs
  a geocoding step (name → lat/lon) before the forecast call — a second
  request and a second thing to cache. Right first candidate to graduate
  from reserved if wttr.in reliability disappoints.
- **`openweathermap` (reserved):** keyed; exists in the kind enum so the
  SecretRef custody rule is fixed now (`api_key = "env:…" | "file:…"`).
- **wego (not a kind):** a client, not a service; its backends need keys.
  Informs nothing beyond the reserved-keyed posture.

### A reserved kind falls back to `none`, not to `wttr_in` (implemented delta)

`config_enum!` deserializes a **present-but-invalid** value by warning and
falling back to the _enum's_ `default`. With `default = WttrIn` a user who
wrote `provider = "open_meteo"` would silently get wttr.in — unexpected egress,
which is worse than useless. So the default sits in two places deliberately:
`None` on the **enum**, `WttrIn` on the **struct** (`#[serde(default)]` on the
container fills a **missing** key from `WeatherConfig::default()`). Result:

- key absent ⇒ `wttr_in`, the sane default;
- `provider = "open_meteo"` ⇒ warn + `none` ⇒ `poll_secs() == None` ⇒ no fetch,
  no thread, no widget — the spec scenario's intent, reached by disabling
  rather than by substituting.

One consequence for the spec text: `thegn doctor` cannot report "reserved" for
a **config-loaded** value, because the value never survives as `open_meteo`.
The `is_reserved()` arm stays in `weather_probes` for shape parity with the
other seams and for a programmatically-constructed config, with that noted in
the code.

### The date/time surfaces, nothing new

The widget renders beside `date`/`clock` (it is of that family: glanceable,
top-right, two-to-five cells); the shipped `[bars] top_right` default gains
`"weather"` immediately before `"date"`, inert until `[weather] enabled =
true` the same way `gpu` is inert without a GPU. Clicking reuses the existing
calendar popup — the widget id joins the `"date" | "clock"` arm of
`widget_detail_inner` — so there is **no new action id, chord, zone, or
popup**, and the help ratchet is satisfied by prose updates to
`bars.md`/`calendar.md`.

Shed order (`fit_stats_cluster`) becomes `["date", "weather", "uptime", …]`:
`date` is softest (the clock carries the same information), weather next — the
user opted in, so it outlives `uptime`/`load`/`freq`, but it is not `cpu`.

The popup block sits **above** `WORLD CLOCKS`: weather is "here, right now"
and the clocks are "elsewhere, right now", which reads in that order and keeps
the clocks anchored at the bottom where existing users expect them. It is a
`Section::Table` (not a `Grid` — a Grid tones the whole value string at once
and flattens the row), and it goes _after_ the agenda's table so `hit()`'s
"the agenda is the first table after the grid" rule still holds; a test pins
that.

### Off-loop, cache-first, quiet failure

- **Wake path:** `hydrate_weather` follows the `hydrate_tracker`/calendar-sync
  pattern — results over a channel, waker pulse; the interval slots into the
  existing ticker (no new timer thread), plus a one-shot slot shortly after
  launch (`WEATHER_FIRST_SLOT`, placed after the usage and startup-fetch
  one-shots so the three don't land on the same tick).
- **`tokio::task::spawn_blocking`, NOT `sched::spawn_bg`** (implemented delta).
  The background lane **silently drops** work when its 8 permits are exhausted,
  on the assumption that a periodic trigger retries shortly. The lane is
  busiest during startup — exactly when the one-shot first poll fires — so the
  widget would stay empty until the next full interval (30 minutes by default).
  Weather is one network-bound task every half hour out of 32 blocking threads;
  it uses `spawn_blocking` directly, the `spawn_usage` precedent. The reasoning
  is repeated in `hydrate_weather.rs`'s module doc so nobody "tidies" it back.
- **Launch:** the state-DB cache is read on the ordinary hydration path (not
  before first frame); network is never touched at startup — the first fetch
  is scheduled, not awaited.
- **Render damage:** a snapshot update sets `bars_dirty` (two-row recompose)
  — same class as the clock tick; with the calendar popup open it takes the
  overlay path. No new damage channel.
- **Failure:** keep last-good, flip to stale past `stale_after_secs` (glyph
  dimmed + age in the popup row). No toasts, no statusbar errors — a weather
  widget that nags is worse than none. `thegn doctor` carries reachability.
- **Refresh floor (600s):** protects wttr.in (which caches ~10–15 min
  anyway) and honors the house rule that a stray `0` can't spin a poll loop.

### Glyphs and degradation

Condition codes map in core to a small glyph _class_ enum (clear, partly,
cloudy, rain, snow, storm, fog, wind…); the host resolves classes through
`caps::active_glyphs()` at the chokepoint, never at draw sites (color/glyph
ratchet). Temperature is plain text either way — `°` (U+00B0) is Latin-1 and
width 1, written directly the way the existing `temp` widget writes it — so
the widget is fully legible on an ASCII terminal.

Chrome policy forbids emoji-presentation glyphs: `⛅` (U+26C5) and `🌧` are
width 2 and are the bug class the policy exists for. The shipped set is eight
BMP, single-width glyphs (`☀ ☼ ☁ ≈ ☂ ☃ ☇ ↝`) with ASCII fallbacks
(`* * = ~ ' # ! ~`); `Sky::Unknown` has **no** glyph and renders the
temperature alone. `unicode_glyphs_are_bmp_and_single_width` is the authority.

### e2e: forced off under the freeze, not pinned (implemented delta)

The tasks said "pin the widget + popup row in `e2e_freeze.rs`". The house
precedent for a network-backed, live-numbers surface is to **disable** it under
the freeze — `[media]`, `[usage]` and `[model_proxy]` all do — because a driven
instance must not reach the network. So `apply_to_config` sets
`cfg.weather.enabled = false`. No baseline changes, and no `just e2e-update`
run is needed.

The same property is asserted directly in the unit tests: with no reading the
popup's sections and its 44-column default width are exactly what they were
(`the_popup_has_no_weather_block_without_a_reading`).

### The cache lives in `ui_state`, not a new table (implemented delta)

The proposal called for a `weather_cache` table and a `user_version` bump. It
does not need either. What is being cached is **one JSON blob per
configuration** — exactly the shape `ui_state` is: a key/value store the host
already opens, already writes best-effort, and already treats as a cache that
git/the provider can rebuild. So:

- key: `thegn_core::weather::cache_key(provider, location, units)` — pure, so
  a read and a write can't disagree about the key;
- value: the serialized `WeatherSnapshot` (normalized, not raw provider JSON,
  so a provider change doesn't invalidate the cache format);
- **`SCHEMA_VERSION` is NOT bumped by this change** — which also sidesteps the
  known collision trap with other in-flight branches, since there is nothing to
  collide over.

The DB is a cache: reads and writes are best-effort. A failed `Db::open` logs
at `debug` and the pass polls without the cache rather than aborting.

### Hard expiry is applied at draw time, not by a timer (implemented delta)

`freshness(fetched_at, now, stale_after, hard_expiry)` is evaluated where the
reading is drawn — in the `weather` widget arm and in `weather_sections` — not
by a task that drops the snapshot when it ages out. Two consequences, both
wanted: the widget and the popup block can never disagree about a reading's
age, and no timer is needed to make an expired reading disappear (the clock
tick already advances `now`).

## Security

- **Egress with user location — the headline risk.** Enabling `[weather]` is
  the consent: with it off (the default) there is zero network activity, zero
  config read beyond the flag. When on, exactly one host is contacted (the
  provider), and the only user data sent is the configured location string —
  or nothing, in which case the provider infers city-level location from the
  request IP (this is stated plainly in `config.toml.example` next to
  `location`). thegn never reads an OS geolocation API, never sends
  coordinates it wasn't given, and never sends anything else (no identifiers,
  no email, no repo data).
- **Credentials:** none for the default provider. Reserved keyed kinds are
  specified now to require SecretRef (`env:`/`file:`) — a raw key in
  `[weather]` is a validation error, not a warning, when that kind lands.
- **Transport: HTTPS only, with nothing to validate (implemented delta).** v1
  exposes no user-configurable provider URL — the wttr.in base is a constant
  (`config_weather::WTTR_IN_BASE = "https://wttr.in/"`) inside the impl. The
  rule survives as that constant and as a code comment; `validate_weather`
  keeps only the checks that can actually fire (units, intervals, `api_key`
  custody, location length). A validation key for a URL the user cannot set
  would be dead code pretending to be a guarantee.
- **Blast radius:** read-only feature — no write surface, no catalog row, no
  new external door. The fetch runs in the host process's background lane
  (not in panes), so sandbox policy is unaffected; offline machines simply
  hold last-good forever.

## Resolved questions

- **`units = "auto"` follows the locale**, not the provider's IP-side default —
  matching the calendar's `auto` week-start precedent, and keeping the
  resolution pure (`resolve_units(pref, locale)` reads
  `LC_MEASUREMENT`/`LC_ALL`/`LANG`: US/LR/MM ⇒ Imperial, else Metric). A
  provider-side default would make the displayed unit depend on where the
  request appeared to come from, which is not a property a user can reason
  about.
- **The forecast strip ships.** The j1 payload carries it for free, and the
  popup is the surface with rows to spare. It is bounded by `show_forecast` /
  `forecast_days` (and by what the provider actually returned — never padded),
  and `draw_table` sizes columns to their widest cell, so the short rows
  collapse on their own.
- **Hard expiry hides, at 24 h by default.** `hard_expiry_secs = 86400`, `0`
  disables it. A machine that was offline for a week shows nothing rather than
  last Tuesday's sun; the rule is applied at draw time (above), so the widget
  and the popup block hide together.
