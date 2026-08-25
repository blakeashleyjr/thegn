# Weather in the date/time surfaces

Linear: THE-46

## Why

THE-46 asks for "weather integration into date/time module". That module is
now real: the `date`/`clock` masthead widgets and the calendar popup behind
them (month grid, agenda, world clocks — `add-calendar-and-world-clock`,
landed; config `[calendar]`, `[[calendar.clocks]]`). Weather is the one
glanceable daily-driver datum that surface still lacks, and the linked tools
show it needs no API key: wttr.in serves one-line and JSON formats keyless
(IP-located by default), and wthrr rides the keyless Open-Meteo API. What does
not exist anywhere in the tree today is any weather code — this is a new,
strictly optional provider seam plus two small display deltas on surfaces
that already exist.

Unlike the calendar (local grid, no network), weather is **network egress
carrying the user's location** — so it is off by default, and enabling it is
the consent step. It must also be honest offline: a cached reading with its
age beats an error glyph.

## What Changes

- **`[weather]` config (all new, documented in `config/config.toml.example`):**
  `enabled` (default **false** — zero default overhead, no egress without
  opt-in), `provider` (`wttr_in` implemented; `open_meteo` and
  `openweathermap` **reserved**), `location` (free-form place string; empty ⇒
  the provider infers from the request IP — documented as such), `units`
  (`auto` | `metric` | `imperial`), `refresh_interval_secs` (default 1800,
  floored at 600 — wttr.in itself caches ~10–15 min), `widget` (show the
  masthead widget, default true when enabled), `stale_after_secs` (staleness
  marker threshold).
- **Provider seam.** `WeatherProvider` in thegn-svc (object-safe, BoxFuture
  ops, house pattern): one `current + short forecast` fetch op. `wttr_in`
  fetches `?format=j1` and the pure decode (JSON → normalized
  `WeatherSnapshot`, unit conversion, condition→glyph class mapping) lives in
  thegn-core under the coverage gate. Probe in `thegn doctor`
  (implemented/unreachable/reserved). Vendor specifics (URLs, j1 field names)
  stay inside the impl file.
- **Masthead widget.** A new `weather` bars widget id (placeable like
  `date`/`clock`, in `top_right` next to them when enabled): condition glyph +
  temperature. Glyphs are mapped through the caps chokepoint
  (`caps::active_glyphs()` — Unicode tier with ASCII fallback; no literals at
  draw sites). Clicking it opens the calendar popup (the existing
  `open-calendar` action — **no new action id**, no new chord).
- **Calendar popup row.** When weather is enabled, the popup shows a weather
  line adjacent to the world clocks: condition, temperature, hi/lo, and a
  compact next-days strip when the provider returned a forecast. Absent (not
  a gap, simply not rendered) when disabled or never-fetched.
- **Off-loop fetch + cache.** Fetching runs on the background hydration lane
  (never on the event loop, never before the first frame), results return
  over a channel with a waker pulse. The last-good snapshot is cached in the
  state DB so launch shows weather instantly with no network; a snapshot
  older than `stale_after_secs` renders with a staleness marker. Fetch
  failures are quiet (keep last-good, mark stale) — reachability lives in
  `thegn doctor`, not in the chrome.
- **e2e:** the widget and popup row are volatile — pinned under `THEGN_E2E=1`
  in `e2e_freeze.rs`.
- **Docs/help:** `docs/help/bars.md` (widget id) and `docs/help/calendar.md`
  (popup row + config) updates satisfy the help-prose ratchet; the
  config-reference page is generated.

## Non-goals

- **Precise geolocation.** thegn never touches an OS geolocation API. Location
  is a user-typed string or provider-side IP inference — nothing more precise
  than the user chose to write down.
- **Keyed providers.** `openweathermap` (and wego-style stacks, which require
  forecast.io/OWM keys) stay reserved; when implemented, keys go behind
  SecretRef (`env:`/`file:`), never raw in config.
- **Weather alerts/notifications, radar, hourly graphs.** The surface is a
  glance row, not a weather app; wthrr/wttr.in remain the right tools for
  more.
- **A weather panel section or dedicated popup.** The date/time surfaces are
  the whole point of THE-46.

## Impact

- Roadmap: belongs to group **AM (daily-driver / non-code tiles)** beside
  AM 473/476; no existing row — the audit phase wires THE-46 in.
- Specs: new `weather` capability (ADDED requirements only). No modification
  to the in-flight `add-calendar-and-world-clock` deltas — this change layers
  a row onto the popup that change built; it depends on that surface (already
  landed on main) and is sequenced after it.
- Code (indicative): `thegn-core/src/{weather.rs,config_weather.rs}`,
  `thegn-svc/src/weather/{mod,wttr_in}.rs`, `thegn-host/src/{bars widget,
hydrate_weather.rs}`, calendar popup render, `e2e_freeze.rs`,
  `seam/registry.rs` probe.
- **SQLite:** one new `weather_cache` table (single-row-per-key last-good
  snapshot) — bump `user_version` by one from the value current at
  implementation time (53 today; in-flight changes also claim bumps, so take
  the next free number then, per the known SCHEMA_VERSION-collision trap).
- Capability catalog: **no new rows** — weather has no externally invokable
  operation; it is chrome fed by a background lane. (If a `thegn weather`
  CLI verb is ever wanted, it enters the catalog in its own change.)
- New dependency: none required — the fetch uses the same HTTP client stack
  the calendar `ics_url` provider uses in thegn-svc; JSON via the existing
  serde stack.
- In-flight overlap: `add-calendar-and-world-clock` (the host surface —
  reconciled above); none other.
