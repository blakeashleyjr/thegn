# THE-485 plan

Scope: CalendarConfig::sync_horizon (fallible, bounded by MAX_DURATION_DAYS, already the schema max); validate_calendar rejects over-limit horizons; host horizon() returns Result and both sync entry points refuse (log + toast) before any provider call or cache write. Coverage already flows via CalendarWindow into apply_page.
Tests: core config_calendar_tests (defaults, max, max+1, u32::MAX, chrono bounds); host hydrate_calendar_tests.
Out of scope: THE-466 caps, THE-462 calendar_ids, replacement-mode redesign (THE-451/452).
