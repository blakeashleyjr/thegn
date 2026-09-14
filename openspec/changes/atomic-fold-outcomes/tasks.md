# Tasks and dependencies

- [x] THE-591: core typed observation/final outcome transaction and private DB tests.
- [x] THE-591: host pre-fold snapshot, projection validation and post-commit lifecycle.
- [x] THE-591: private queued-rejecting/write-failure triggers and stale-state regressions.
- [x] Independent source review and coordinated core/host focused gates.
- [x] THE-589: retain previously passing 39-test fold outcome behavior after integration.
- [x] Integrate THE-588's commit-bound `apply_landed` call and pass the combined scoped core/host gate.
- [ ] THE-588: separate destructive cleanup ownership safeguards before native queue retry.
- [x] Complete normal native/full workspace gate and user-authorized local-main landing.

Component evidence: core `db_aux::merge_outcome_tests` passed 13/13;
host `integrate::` plus `handlers::merge_queue::` passed 49/49. The final
directional-mark sanitizer refinement passed its 2/2 focused regressions.
`just ratchets` passed. The first host run's two fixture FK failures are retained
in the audit log; the repaired fixture creates its private workspace parent.

This is a reviewed component checkpoint, not native queue or main-merge approval.
The standalone lifecycle call remains compatible with the base API; the separate
THE-588 integration patch must replace it with the exact committed-result API
before the combined gate. No native integrate, sweep or live-state operation was
used by these tests.

## Landed acceptance — 2026-09-14

Local main `1ef8228f` contains the commit-bound `apply_landed` integration with
actual result OIDs. The historical standalone-call limitation above is superseded.
Independent review verified all 14 current atomic DB-outcome tests and all eight
host persistence cases passed, including queued-rejecting triggers, nullable
clears, rollback, concurrent ownership changes and commit-before-lifecycle order.
The two formerly environment-blocked speculative-land regressions also passed
in `/tmp/thegn-native-unblocked-host-results.json`. The normal CLI built from
`1ef8228f` passed the private 106-command integration fixture, including failed
gates without transient queued/landed writes and successful retained folds.
Evidence is retained at `/tmp/thegn-integrate-native-tw5m0z40/evidence` and the
September 14 local landing audit. THE-591's scoped repair is accepted; remaining
cleanup ownership contracts and native queue operations
are separate obligations.
