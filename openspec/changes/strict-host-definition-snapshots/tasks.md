## Recovered core source

- [x] Recover bounded immutable snapshot data and static typed errors from retained `9d744a1206f4a32c67295cf69de546e473f91215`.
- [x] Recover duplicate-aware raw JSON validation before permissive HostConfig decode.
- [x] Recover the object-safe strict HostStore method without changing legacy readers.
- [x] Recover version/schema checks and bounded rows captured in one main-qualified read transaction.
- [x] Recover private storage, bounds, schema, malformed-state and WAL-interleaving regressions.
- [x] Add actual checked-store capture refusal for malformed persisted rows shadowed by an unchanged declarative Local host, with a valid persisted-SSH positive control.
- [x] Register reciprocal THE-602 delivery ownership without claiming full issue completion.

## Current verification and delivery

- [ ] Complete primary and independent review of the recovered current-source candidate and address findings.
- [ ] Compile and run the focused core snapshot/schema/HostStore tests in the coordinated native lane.
- [ ] Run source/OpenSpec/delivery gates, strict lint and required local component checks.
- [ ] Record exact current source and raw test receipts before any scoped local landing.
- [ ] Reconcile THE-602 partial implementation and remaining acceptance; do not mark Done from the strict reader alone.

## Retained historical evidence

The original tasks recorded a 50/50 focused-core pass, including 22 snapshot
regressions, under nextest run `5f3a5c93-0737-49af-8961-4c810487e392`, and
historical root/peer reviews. The raw test receipt was not recovered with this
component. Those statements remain historical claims, not verification of this
recovered candidate or its new regression. Current review and native gates above
remain unchecked until separately completed.

## Remaining issue acceptance

The strict reader returns captured source-validity data, not a finalized config
or launch permission. THE-598 owns effective winning-host environment synthesis.
Actual final semantic validation after trusted host augmentation, before usable
checked configuration is published, remains outstanding and cannot be replaced
by a test-only launch callback. Host opening/absence handling (THE-603), external
store compatibility (THE-604), final checked launch composition (THE-592) and
runtime containment remain separate obligations. No provider or launch execution
is performed by this storage component or its tests.
