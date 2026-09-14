# Tasks and dependencies

- [x] THE-591: core typed observation/final outcome transaction and private DB tests.
- [x] THE-591: host pre-fold snapshot, projection validation and post-commit lifecycle.
- [x] THE-591: private queued-rejecting/write-failure triggers and stale-state regressions.
- [x] Independent source review and coordinated core/host focused gates.
- [x] THE-589: retain previously passing 39-test fold outcome behavior after integration.
- [ ] Integrate THE-588's commit-bound `apply_landed` call and gate the combined tree.
- [ ] THE-588: separate destructive cleanup ownership safeguards before native queue retry.
- [ ] Normal native gate and explicit approval before any local-main merge.

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
