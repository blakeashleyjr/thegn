# Tasks — weather in date/time surfaces (THE-46)

- [x] Add the pure snapshot/forecast domain, condition classes, unit conversion,
      staleness, hard expiry, and tolerant wttr.in decoding.
- [x] Add validated, opt-in `[weather]` configuration and document location/IP
      inference and reserved providers.
- [x] Add the object-safe provider seam, wttr.in implementation, and doctor
      probe with reserved/unreachable states.
- [x] Cache the last good bounded JSON snapshot in `ui_state` without a schema
      bump.
- [x] Fetch off-loop through the existing hydration/ticker channel and waker,
      with no disabled-state worker.
- [x] Add the bars widget and bounded calendar weather block, including stale/
      expiry, click, narrow-width, and existing-action behavior.
- [x] Force weather off under the e2e freeze and test unchanged default frames.
- [x] Update configuration/help/roadmap records and focused tests.
- [x] Land and review the implementation series (`dd3b0878` through
      `846c3929`) and strictly validate this change.

## Validation boundary

The historical branch did not record a distinct heavy full-test/coverage run;
this record does not manufacture one. Focused domain/provider/host/render tests
and review fixes are evidenced.
