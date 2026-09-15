# THE-639 mutant-1 independent review

The completed mutant-1 evidence is source and log consistent. Source commit `ccb10143a60e5ec80f52b7698f6c2d1b656d4a9f` and both receipt-pinned source files match their recorded SHA-256 values. The pinned `host-tests` binary (`3bce4e8d6dab1df0a3021426f0b5b61a4dba853b44542776932a965684c14b2e`) and `host-tests.d` depfile also match their receipt values. The depfile contains both tested source paths and the absolute mutant worktree provenance.

The reused build completed successfully (`exit 0`, 535 seconds) under the test profile. Its JSONL identifies the `thegn` target artifact as `fresh: false`, so this review preserves the receipt’s completed-build reuse claim rather than treating it as a fresh compilation.

The native test ran the exact selector and produced the expected mutant semantic failure (`exit 101`, 0 passed, 1 failed, 0.24 seconds). The first failing assertion is `devcontainer_startup_tests.rs:1144`: the `Point::BeforeSpawnClaim` case expected `spawn_calls == 0` and observed `1`. The explicit `h.fixture.finish()` at line 1143 occurs before that assertion, so the first-case cleanup/settlement step was reached. Because the test aborts there, the `Point::SpawnClaimed` after-claim case and its later assertions were not reached and are not claimed as exercised.

No build, native rerun, performance run, or source modification was performed for this review. The JSON record beside this report contains the exact receipt, artifact, log, provenance, and reachability details.
