# Tasks and dependencies

- [x] THE-597: typed nonblocking owned lock and retained physical directory checks.
- [x] THE-597: exact root/linked checkout association and pinned commit checks.
- [x] THE-597: remove implicit purge/prune/force-create and guard throwaway cleanup.
- [x] THE-597: private concurrency, failure, alias, stale and reassignment tests.
- [x] THE-597: shared bounded probe runner extraction and cleanup regressions.
- [x] THE-597: hidden-index/sparse refusal and fresh private-index materialization.
- [x] THE-597: owned blocking writer waits, unknown-wait poison and safe index cleanup.
- [x] THE-597: preserve gate-authorized filters; used/unused/failure regressions.
- [x] Independent source review and scoped host regression gate.
- [ ] Combined THE-588/THE-589/THE-591/THE-595 gate and fixed private CLI proof.
- [ ] THE-601: separately bound initial process capture and gate cancellation.

Source6 diagnostic host gate passed 139/139 (Nextest
20d042e9-199b-43c0-a477-56e9981a5926); later fsmonitor/stat-cache findings prevent
that result from approving materialization. Independently reviewed source9 passed
145/145 host tests (3056 skipped), Nextest 421794e9-558a-43fe-9d82-05ef9e8fb778,
including fresh-index/writer ownership, filters, replacement refs and cleanup.
The parent test environment explicitly set GIT_NO_REPLACE_OBJECTS=1, matching the
native gate environment; construction assertions independently require explicit
production setters. Build took 4m18s; tests took 34.268s. Source9 hashes remained
unchanged through the run, and source ratchets passed. This Linux scoped result
does not replace the full combined gate, native CLI proof or THE-606 history
admission prerequisite. No live queue result is claimed.
