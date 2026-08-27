# Tasks — weather in the date/time surfaces

## 1. Pure core domain

- [x] 1.1 `weather.rs`: `WeatherSnapshot`/`ForecastDay` model, condition →
      glyph-class enum, °C/°F + wind unit conversion, staleness/hard-expiry
      math.
- [x] 1.2 wttr.in `?format=j1` decode (pure JSON → model), tolerant of
      missing fields; table tests against captured fixtures to the 95% gate.
- [x] 1.3 `config_weather.rs`: `[weather]` keys, `WeatherProviderKind`
      `config_enum!` (bump the pinned marked-definition count deliberately),
      refresh floor at 600 in the accessor (not the ticker), `units` enum,
      SecretRef validation in `config_validate` with warnings. (No HTTPS-URL
      check: v1 exposes no configurable provider URL — see design § Security.)
- [x] 1.4 Document every key in `config/config.toml.example`, including the
      empty-location ⇒ IP-inference egress note beside `location`.

## 2. Provider seam (svc)

- [x] 2.1 `weather/mod.rs`: object-safe `WeatherProvider` (BoxFuture fetch
      op), `provider_for()` by kind; reserved kinds inert.
- [x] 2.2 `weather/wttr_in.rs`: URL building (path location, `?format=j1`,
      units flag), single GET via the existing svc HTTP stack, decode via
      core. Vendor strings stay in this file.
- [x] 2.3 `seam/registry.rs`: weather probe (disabled / implemented /
      unreachable-with-reason / reserved); extend the registry tests.

## 3. Cache

- [x] 3.1 Last-good snapshot cached as a `ui_state` entry keyed by
      `weather::cache_key(provider, location, units)`, best-effort both ways.
      _(Replaces the proposal's 3.1/3.2: the value is one JSON blob per
      configuration, which is what `ui_state` already is — so there is no
      `weather_cache` table and `SCHEMA_VERSION` is **not** bumped. Design
      § "The cache lives in `ui_state`".)_

## 4. Host wiring

- [x] 4.1 `hydrate_weather.rs`: cache-read on the hydration path, interval
      slot + a launch one-shot in the existing ticker, fetch via
      `tokio::task::spawn_blocking` (NOT `sched::spawn_bg`, which silently
      sheds work exactly when the first poll fires — design § Wake path),
      channel + waker delivery; a snapshot update sets `bars_dirty` only, and
      only when the reading actually changed.
- [x] 4.2 `weather` bars widget: glyph via the `caps::active_glyphs()`
      chokepoint + temperature; stale dims; hidden past hard expiry; shipped in
      the `[bars] top_right` default (inert until enabled) and shedding right
      after `date`; click opens the existing calendar popup, no new action id.
- [x] 4.3 Calendar popup `WEATHER · <place>` block above the world clocks:
      condition, temp, feels-like, hi/lo, humidity, wind, plus the forecast
      strip bounded by `show_forecast`/`forecast_days`; absent entirely when
      disabled, never fetched, or expired; the popup only widens when the block
      is actually drawn.
- [x] 4.4 `e2e_freeze.rs`: weather forced **off** under `THEGN_E2E=1` — the
      `[usage]`/`[media]` precedent for a network-backed surface — rather than
      pinned. No baseline changes.
- [x] 4.5 Render-plan tests: snapshot arrival ⇒ bars-only damage; popup-open
      update ⇒ overlay path; disabled ⇒ no ticker slot at all.

## 5. Docs + wrap-up

- [x] 5.1 `docs/help/bars.md` (widget id + degradation) and
      `docs/help/calendar.md` (weather block, `[weather]` pointer) — satisfy
      the help-prose ratchet; config-reference page is generated.
- [x] 5.2 e2e: nothing to re-record — weather is off under the freeze, and a
      test asserts the popup's sections and default width are unchanged
      without a reading.
- [x] 5.3 `tasks.md` roadmap row 760 under group AM.
- [x] 5.4 Reconcile this change's artifacts with what was implemented
      (the five deltas above), then `just openspec-validate`.
- [ ] 5.5 Run the change's final gates once: `THEGN_ALLOW_HEAVY=1 just test`
      and `THEGN_ALLOW_HEAVY=1 just coverage`.
