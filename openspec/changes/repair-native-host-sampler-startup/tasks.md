# Tasks

- [x] 1. Trace the native sampler worker, activation/park/drop path, and
      platform collector initialization that preceded the first park.
- [x] 2. Defer sampler initialization until the worker observes active state;
      preserve first-sample and resume priming behavior.
- [x] 3. Make host lifecycle tests use a deterministic injected sampler and add
      gated-factory coverage for stop and hide during startup, including stop
      and hide after activation from an initially parked state, with no
      priming/read on observed stop or park and one collector across reactivation.
- [ ] 4. On build-mac, run the same four original `gtui-query` cases repeatedly
      in serial and concurrently on baseline and repair; confirm exact selected
      names/counts, capture a native sample witness, and verify bounded
      park/drop. Include the three repair-only factory regressions on repair.
- [ ] 5. Run the repository's required local gates when permitted, including
      `just quick gtui-query` and focused `cargo nextest` coverage.
