# Tasks

- [x] 1. Trace the native sampler worker, activation/park/drop path, and
      platform collector initialization that preceded the first park.
- [x] 2. Defer sampler initialization until the worker observes active state;
      preserve first-sample and resume priming behavior.
- [x] 3. Make host lifecycle tests use a deterministic injected sampler and add
      proof that initialization waits for activation.
- [ ] 4. Run the four affected `gtui-query` tests repeatedly in serial and
      concurrently on build-mac at baseline and repair; capture a native sample
      witness and verify bounded park/drop.
- [ ] 5. Run the repository's required local gates when permitted, including
      `just quick gtui-query` and focused `cargo nextest` coverage.
