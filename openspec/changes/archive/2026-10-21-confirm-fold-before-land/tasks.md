# Tasks and dependencies

- [x] THE-589: distinguish preparation from successful CAS landings.
- [x] THE-589: guard persistence/lifecycle and retain bounded diagnostic phases.
- [x] THE-589: preserve CLI/UI outcome semantics; remove implicit expiry sweep.
- [x] THE-589: private-repository gate/base/bisect/CAS/Ready/persistence regressions.
- [x] Independent source review and scoped gates.
- [x] Combined safety integration and normal native merge gate.
      Superseded, not performed as a separate step: THE-589 closed Done on 2026-09-14 after the combined safety integration landed on local main (7014a496) with a full native receipt of 8,405/8,405 tests; every maintenance batch since has landed through the same fold gate.
- [x] THE-588 dependency: independently authenticate explicit cleanup scope.
      Done in THE-588 (closed Done); tracked there, not in this change.
- [x] THE-591 dependency: pre-fold observations and atomic final-row persistence.
- [x] THE-586 retry only after both safety repairs are accepted.
      Done in THE-586 (closed Done); tracked there, not in this change.

The THE-589-only snapshot passed 39/39 focused host tests. The subsequent
THE-591 integration passed 13/13 core and 49/49 host tests; a final sanitizer
refinement passed its 2/2 focused regressions. Source ratchets passed. Native
queue retry and main-merge approval remain blocked on the separate THE-588
commit-bound lifecycle API integration and the combined gate.
