# Tasks and dependencies

- [x] THE-606: shared original-identity local canonical-history admission.
- [x] THE-606: refuse replacements, graft/shallow metadata and ambient overrides.
- [x] THE-606: cover discovery, snapshots, fold operations, no-op publication and CAS.
- [x] THE-606: override gate blame on observed history mutation, retaining diagnostics.
- [x] THE-606: retain canonical-history proof through automatic cleanup callbacks.
- [x] THE-606: private initial/post-callback and unsupported transport/platform tests.
- [x] Independent source review, source ratchets and focused host regression gate.
- [x] Record representative focused timing and added probe cost without weakening proof.
- [ ] Combined source review, full native gate and private CLI proof before delivery.

Source2 focused gate: 203/203 passed, 3015 skipped, exit 0; nextest run
`9c8b3d7f-0567-4320-8cfd-ef902f34e2b9`. Build 3m37s, tests 44.384s.
Evidence: `/tmp/thegn-audit/tasks/the606-focused-host.log`. This is a regression
run, not a controlled performance comparison. Each canonical revalidation adds
two bounded Git path probes and one bounded replacement-ref probe plus metadata
and optional registry checks. No completed-source quadratic revalidation is added.

The subsequent source3 delta marks two now-test-only fold wrappers with cfg(test)
and corrects their documentation. Its full configured gate passed the three
contract stages, then failed an old daemon test expecting automatic integration
of an unverified provider source: 4746 passed, 1 failed, 3401 not run, 23 skipped.
This is the explicitly unsupported THE606 transport, not a local-fold failure.
Both transport fixtures now retain real enqueue proof (including HTTP/authentication
in the daemon test), with an explicit stub for the exact enqueue branch query.
After enqueue, the same provider is armed to reject any execution; drain asserts
the specific provider-history refusal, unchanged query audit, and unchanged Git
state. The combined THE606/THE608/THE610 full configured `just test` subsequently
passed all three contract stages (2 + 2 + 1) and 8158/8158 workspace tests, with
23 existing configured skips, run `3c0c9464-54f4-4773-ba05-2a6d01f713de`.
Build 4m51s, tests 201.228s; all 29 candidate hashes matched before/after.
Evidence: `/tmp/thegn-audit/tasks/the608-full3-just-test.log`, SHA256
`318aed8cf9ca3916fe2428c4a864686687a22c59fdcb3a66ff7536527aadd764`.
This task-record update is documentation-only after testing; fixed CLI/native
rehearsal and live queue integration still remain pending.
No native test pass or live queue outcome is claimed. Private Git mechanics
evidence exists separately under
`/tmp/thegn-audit/the597-ancestry.1jRz0X`; it establishes the original Git behavior,
not runtime validation of this implementation.
