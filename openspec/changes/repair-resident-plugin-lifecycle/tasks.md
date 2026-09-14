## Scoped liveness repair

- [x] Investigate native pipe/child ownership and obtain primary approval.
- [x] Add bounded serialized admission and independent terminal control.
- [x] Add tracked owner/held custody and cancellation-safe task observation.
- [x] Add Unix nonblocking pipes and non-reaping group identity observation.
- [x] Add Windows finite owned pipe workers and repeated exact-thread cancellation.
- [x] Bind provider pending calls and replies to their originating session.
- [x] Wire one supervisor across reloads and cleanup outside all UI-loop returns.
- [x] Pass focused native Unix session/provider/lifecycle fixtures (final assembled service plugin suite 48/48 passed).
- [x] Pass host integration and required source/format checks (assembled tests, scoped clippy and source/format gates; native platform acceptance remains below).
- [x] Complete independent adversarial review of the final checkpoint (53502768).
- [x] Coordinate scoped local landing (ea807922; native acceptance below remains open).

## Full THE-154 acceptance remains outstanding

- [ ] Execute native Windows cancellation/admission/lifecycle fixtures.
- [ ] Supply enforceable escaped-descendant containment and platform failure policy.
- [ ] Prove complete process-tree cleanup with native platform evidence.

## September 14 native verification follow-up

- [x] Primary approval for portable test-only reexec fixtures and explicit native Windows CI selection; no containment implementation.
- [x] Add explicit READY, finite fixture lifetime, assertion-unwind cleanup and shell-independent lifecycle/failure coverage.
- [x] Select native lifecycle and exact Windows cancellation-race regressions in the existing opt-in CI job; preserve macOS pipeline.
- [x] Execute eight new native Linux regressions plus all 48 existing plugin tests; exclude one ignored helper from the count (two platform ratchets also passed).
- [x] Pass the full Windows service library/test crosscheck with the new fixture source; this is not native execution.
- [ ] Check the new Darwin fixture graph; the historical unix.rs-only harness does not cover it. Native Windows/macOS execution still requires their kernels.
- [x] Complete independent adversarial review, scoped checks and local landing of the follow-up.
