# Tasks

- [x] Investigate shared ticker, all listed signed duration casts, config layering/write/schema seams, and provider ledger time provenance.
- [x] Obtain primary plan approval and incorporate ceiling-division, zero-lifetime, and security-config fallback revisions.
- [x] Add checked runtime time primitives and document numeric field-family bounds.
- [x] Enforce duration-only strict validation, pre-write checks, and atomic explicit-overlay admission.
- [x] Preserve successfully parsed base-file security configuration while reporting invalid duration policy.
- [x] Replace shared cadence conversion and add unwind-only worker failure notification.
- [x] Run first focused core gate: 28 tests pass; primitive debug/optimized and worker-failure harnesses pass.
- [x] Complete independent model/PR scheduler regression; nine actual-source cadence/schedule/panic fixtures pass in debug and optimized builds.
- [ ] Run updated config hot-path parity/cache tests, measure full-load overhead, and complete primary/adversarial review of THE-483.
- [ ] Replace remaining signed duration/epoch policy casts and classify all remaining casts.
- [ ] Implement and test visible fail-closed provider time quarantine and any trustworthy generation-bound fallback.
- [ ] Verify daemon lease, MCP breaker, model budget, usage reset, and reaper action-count boundaries.
- [ ] Complete combined host/service checks, formatting/spec/delivery gates, and independent adversarial review.
- [ ] Land reviewed changes on canonical local main when Git metadata is writable.
