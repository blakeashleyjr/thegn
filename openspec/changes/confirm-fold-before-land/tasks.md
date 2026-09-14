# Tasks and dependencies

- [x] THE-589: distinguish preparation from successful CAS landings.
- [x] THE-589: guard persistence/lifecycle and retain bounded diagnostic phases.
- [x] THE-589: preserve CLI/UI outcome semantics; remove implicit expiry sweep.
- [x] THE-589: private-repository gate/base/bisect/CAS/Ready/persistence regressions.
- [x] Independent source review and scoped gates.
- [ ] Combined safety integration and normal native merge gate.
- [ ] THE-588 dependency: independently authenticate explicit cleanup scope.
- [x] THE-591 dependency: pre-fold observations and atomic final-row persistence.
- [ ] THE-586 retry only after both safety repairs are accepted.

The THE-589-only snapshot passed 39/39 focused host tests. The subsequent
THE-591 integration passed 13/13 core and 49/49 host tests; a final sanitizer
refinement passed its 2/2 focused regressions. Source ratchets passed. Native
queue retry and main-merge approval remain blocked on the separate THE-588
commit-bound lifecycle API integration and the combined gate.
