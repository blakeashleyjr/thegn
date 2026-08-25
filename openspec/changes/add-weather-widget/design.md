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
  hydrate_weather.rs  — background lane: cache-read at start, fetch on
                        interval, channel + waker delivery
  bars: `weather` widget · calendar popup weather row · e2e_freeze pin
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

### The date/time surfaces, nothing new

The widget renders beside `date`/`clock` (it is of that family: glanceable,
top-right, two-to-five cells). Clicking reuses `open-calendar` — the weather
detail row lives in the popup the user already knows, so there is **no new
action id, chord, zone, or popup**, and the help ratchet is satisfied by
prose updates to `bars.md`/`calendar.md`. The popup row sits adjacent to the
world clocks: the same "elsewhere right now" register.

### Off-loop, cache-first, quiet failure

- **Wake path:** `hydrate_weather` follows the `hydrate_tracker`/calendar-sync
  pattern — a background lane task, results over a channel, waker pulse; the
  interval slots into the existing ticker (no new timer thread). Thread QoS:
  `Background`.
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
`caps::active_glyphs()` — Unicode tier (☀ ⛅ ☁ 🌧 …) vs ASCII fallback — at
the chokepoint, never at draw sites (color/glyph ratchet). Temperature is
plain text either way, so the widget is fully legible on an ASCII terminal.

### SQLite

`weather_cache(provider, location_key, units, fetched_at, payload)` — one
last-good normalized snapshot per key (not raw provider JSON, so a provider
change doesn't invalidate the cache format). `user_version` bumps by one from
whatever is current when this lands (53 today; other in-flight changes claim
bumps — take the next free number, the known collision trap). The DB is a
cache: all writes best-effort per house rules.

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
- **Transport:** HTTPS only; a plain-http provider URL is rejected at
  validation.
- **Blast radius:** read-only feature — no write surface, no catalog row, no
  new external door. The fetch runs in the host process's background lane
  (not in panes), so sandbox policy is unaffected; offline machines simply
  hold last-good forever.

## Open questions

- Should `units = "auto"` follow locale (`LC_MEASUREMENT`) or the provider's
  IP-side default? Leaning locale, matching the calendar's `auto` week-start
  precedent.
- Is a 3-day strip in the popup worth its rows, or is current + hi/lo enough
  for v1? The j1 payload carries it either way; rendering can start minimal.
- Whether the widget should hide entirely (vs render stale) after some
  hard-expiry age (e.g. 24h) — proposed: hide after 24h, so a machine that
  was offline for a week doesn't show last Tuesday's sun.
