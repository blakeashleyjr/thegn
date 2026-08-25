# Tasks — weather in the date/time surfaces

## 1. Pure core domain

- [ ] 1.1 `weather.rs`: `WeatherSnapshot`/`ForecastDay` model, condition →
      glyph-class enum, °C/°F + wind unit conversion, staleness/hard-expiry
      math.
- [ ] 1.2 wttr.in `?format=j1` decode (pure JSON → model), tolerant of
      missing fields; table tests against captured fixtures to the 95% gate.
- [ ] 1.3 `config_weather.rs`: `[weather]` keys, `WeatherProviderKind`
      `config_enum!` (bump the pinned marked-definition count deliberately),
      refresh floor at 600 in the accessor (not the ticker), `units` enum,
      HTTPS/SecretRef validation in `config_validate` with warnings.
- [ ] 1.4 Document every key in `config/config.toml.example`, including the
      empty-location ⇒ IP-inference egress note beside `location`.

## 2. Provider seam (svc)

- [ ] 2.1 `weather/mod.rs`: object-safe `WeatherProvider` (BoxFuture fetch
      op), `resolve()` by kind; reserved kinds inert.
- [ ] 2.2 `weather/wttr_in.rs`: URL building (path location, `?format=j1`,
      units flag), single GET via the existing svc HTTP stack, decode via
      core. Vendor strings stay in this file.
- [ ] 2.3 `seam/registry.rs`: weather probe (disabled / implemented /
      unreachable-with-reason / reserved); extend the registry tests.

## 3. Cache (state DB)

- [ ] 3.1 `weather_cache` table keyed by (provider, location_key, units)
      storing the normalized snapshot + `fetched_at`; best-effort writes.
- [ ] 3.2 Bump `user_version` by one from the value current at implementation
      time (53 today — check for in-flight collisions first, the known
      SCHEMA_VERSION trap).

## 4. Host wiring

- [ ] 4.1 `hydrate_weather.rs`: cache-read on the hydration path, interval
      slot in the existing ticker, fetch on the background lane
      (`Background` QoS), channel + waker delivery; snapshot update sets
      `bars_dirty` only.
- [ ] 4.2 `weather` bars widget: glyph via `caps::active_glyphs()` chokepoint + temperature; stale dimming; hidden past hard expiry; click →
      `open-calendar` (reuse, no new action id).
- [ ] 4.3 Calendar popup weather row beside the world clocks: condition,
      temp, hi/lo, compact forecast strip; absent when disabled/never
      fetched; degrades on narrow popups.
- [ ] 4.4 `e2e_freeze.rs`: pin widget + popup row under `THEGN_E2E=1`.
- [ ] 4.5 Render-plan tests: snapshot arrival ⇒ bars-only damage; popup-open
      update ⇒ overlay path; disabled ⇒ no wake source (idle-guard green).

## 5. Docs + wrap-up

- [ ] 5.1 `docs/help/bars.md` (widget id + degradation) and
      `docs/help/calendar.md` (weather row, `[weather]` pointer) — satisfy
      the help-prose ratchet; config-reference page is generated.
- [ ] 5.2 e2e: re-record any baseline the enabled-widget cases touch
      (`just e2e-update`); default-off means default baselines are untouched.
- [ ] 5.3 Run `just ci` once (includes openspec validate).
