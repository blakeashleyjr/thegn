# Tasks

- [x] 1. Trace the native sampler worker, activation/park/drop path, and
     platform collector initialization that preceded the first park.
- [x] 2. Defer sampler initialization until the worker observes active state;
     preserve first-sample and resume priming behavior.
- [x] 3. Make host lifecycle tests use a deterministic injected sampler and add
     gated-factory coverage for stop during both active start and activation
     from an initially parked state, plus hide during both startup states.
     Both stop cases require drop to return while the factory remains held,
     then release it and require worker exit with zero priming or first reads.
     Hide cases require no read while parked and one collector across
     reactivation.
- [ ] 4. On build-mac, run the saved incident baseline command
     `b193-native-sampler-diagnostic-a454cbf45` on both SHAs at one and four
     threads. It selects exactly
     `dropping_an_idle_sampler_wakes_and_joins_it_promptly`,
     `a_parked_sampler_stops_sampling_and_resumes_on_activation`,
     `dropping_a_parked_sampler_actually_exits_the_worker_promptly`, and
     `empty_ring_is_reported_as_partial_retention`. Confirm resolved names and
     counts and repeat the four-case comparison six times. On the candidate,
     also run the exact all-host-family command below; it selects every
     `host::tests` case, including initialization and all stop/hide regressions.
     Capture a native sample witness and verify bounded park/drop.
- [ ] 5. Run the repository's required local gates when permitted, including
     `just quick gtui-query` and focused `cargo nextest` coverage.
