# Tasks

- [x] Investigate shared ticker, all listed signed duration casts, config layering/write/schema seams, and provider ledger time provenance.
- [x] Obtain primary plan approval and incorporate ceiling-division, zero-lifetime, and security-config fallback revisions.
- [x] Add checked runtime time primitives and document numeric field-family bounds.
- [x] Enforce duration-only strict validation, pre-write checks, and atomic explicit-overlay admission.
- [x] Preserve successfully parsed base-file security configuration while reporting invalid duration policy.
- [x] Replace shared cadence conversion and add unwind-only worker failure notification.
- [x] Run first focused core gate: 28 tests pass; primitive debug/optimized and worker-failure harnesses pass.
- [x] Complete independent model/PR scheduler regression; nine actual-source cadence/schedule/panic fixtures pass in debug and optimized builds.
- [x] Run updated core32 and existing config293 tests, and measure full-load overhead (+0.21% median, within noise).
- [x] Complete THE-483 primary/adversarial source approval and combined host cadence/panic fixtures at checkpoint 2; native batch-wide gates remain separate.
- [x] Replace remaining signed duration/epoch policy casts and classify all remaining cast files in the THE-484 audit.
- [x] Implement and test visible provider-time quarantine with exact Fly custody/inventory evidence; never invent local generation/time provenance.
- [x] Verify daemon lease, MCP breaker, model budget and usage reset boundaries: core118 + extra26, svc28, proxy/media26 pass; actual-source shared reaper admission action-count fixture passes.
- [x] Complete primary and independent source review at 2dbe30aa after custody, ambiguous environment policy and empty VPS ID revisions; focused service checks, formatting, idle guard, ignored-result ratchet and strict OpenSpec pass.
- [ ] Complete final combined host checks and delivery gates; broad/native provider lifecycle gates remain separately required.
- [ ] Land reviewed changes on canonical local main when Git metadata is writable.

## THE-483 running-worker acceptance follow-up

- [x] Obtain primary approval for one shared production spawner/loop with statically dispatched owned I/O boundaries.
- [x] Add bounded channel-clock fixtures for each hostile cadence family, disabled schedules, and ordinary startup/periodic coalescing.
- [ ] Run actual host worker fixtures and counterfactual unchecked-overflow failure; record exact evidence.
- [ ] Complete primary and independent review and land the acceptance follow-up.
