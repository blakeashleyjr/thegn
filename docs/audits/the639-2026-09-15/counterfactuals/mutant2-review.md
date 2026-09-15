# THE-639 mutant2 independent native review

Verdict: **evidence accepted for the intended counterfactual failure; no
source or provenance blocker found**.

The pinned source clone is
`/tmp/thegn-THE639-native-02-local-displaced-session-on-unwind-20260915` at
`4bf53c348e36b0cb6b8a88d32698ab4c0c0e26b5`, clean at review time. The receipt's
five source hashes match that checkout exactly. This is an isolated
nonshipping counterfactual: its production publish path is intentionally
mutated to retain `map.insert`'s displaced value in `retired` after the
injected post-insert assertion, and it adds the fixture release/finish calls.
No shipping behavior is claimed.

The pinned binary SHA is
`c48d10add226ed3cd580196a1bc502ee6aa1cecb899b095db5201eeac158b339`; its
depfile SHA is
`1d22a1654fd03b9331bc6a123d8fa3ab5b62b7423fcda5df07da01cf05a6072d`. The
reused build metadata is `/tmp/thegn-THE639-mutant2-build-20260915.jsonl`
(SHA `17d2825958efe7e4347a15ec8763b6994cb4487eb50d19e23ff4d6712789947d`) and
its log (SHA
`6a8c1a7aa8a608ca4f1485103e536c79bec5e7973ccc90ffd47e809fbe078155`) records
successful completion with `THEGN_GIT_SHA=4bf53c34`. The receipt explicitly
records `compiler_artifact_fresh: false`: this review did not compile or
execute, and the recorded artifact is the root's reused build rather than a
fresh artifact.

The native selector was
`devcontainer_provider::startup::tests::unix::dropped_success_and_immediate_post_insert_unwind_keep_custody`.
The run exited 101 as expected. The hook is set at test-source line 748 and
the injected publisher panic occurs at `devcontainer_startup.rs:778`. The
target scenario's `fixture.finish()` at test-source line 758 returned; the
HELD and poisoned-retired checks at lines 759–760 passed. The first semantic
failure is the intended retired-ownership assertion at line 761:
`operation.retired.lock().unwrap_err().into_inner().is_some()`. The later
registry, snapshot, destructor, and Busy assertions at lines 762 onward were
unreached and are not claimed by this receipt. The raw log SHA is
`f87e22903489687b9f42fd71d50718a71fc49700efe5e27abcfa7ba29d32f12a`.

No build, test, rerun, native execution, or source edit was performed during
this independent review. The result is limited to the pinned isolated
counterfactual's first semantic failure and its recorded source/artifact
provenance; later assertions and shipping behavior are not claimed.
