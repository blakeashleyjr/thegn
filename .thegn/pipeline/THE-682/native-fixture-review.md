# THE-682 native fixture review

Verdict: APPROVED

Reviewed `b73748b5` against parent `2a966cfd8d9ec73dbecca27026c27196b79cf3ed`,
with the production implementation in `scripts/live.py`,
`test/live_test.py`, `CLAUDE.md`, and the native fixture handoff.

The repair is correctly scoped and keeps the assertions meaningful:

- `private_quiescence` captures `live.quiescent` before patching the module and
  delegates to that real implementation with an empty private proc directory.
  It does not replace or weaken the production process-inspection logic.
- The context manager is used only by the two `live.main()` orchestration
  fixtures: plan/confirmation and supervisor/launch. The direct process
  ownership fixtures, controller-name refusal cases, and prebuild preflight
  lock-recheck fixture still call their intended implementations independently.
- Process ownership and busy-controller refusal remain covered by the explicit
  proc fixtures, including open database descriptors, unknown/other-UID
  processes, namespaced non-thegn processes, and namespaced thegn refusal.
  Lock ownership and release remain asserted by the nested busy-lock checks and
  the post-preflight/post-child lock acquisitions.
- The plan test still checks a filesystem snapshot, rejects non-terminal and
  negative confirmation paths, verifies no build call, and preserves the old
  target. The supervisor test still verifies all three install locks, launcher
  lock lifetime, schema-lock release, child environment, and both migration-pin
  cases. The late preflight refusal test remains a separate direct
  `install()` test with the third observation refusing before replacement.

The native retry script also retains the current `just test` coverage. It runs
both Python suites, all three explicit contract selections, the full workspace
nextest suite, and ratchets. Each Rust selection uses `--workspace` and
`--no-tests fail`; the shared workspace command shape avoids package-specific
feature graphs while the full workspace run still executes all tests. The
retry additionally runs scoped all-target clippy for `thegn-host`,
`thegn-svc`, and `thegn-core`. No contract selection or workspace coverage is
dropped by that build-graph sharing.

Verification performed:

- `python3 test/live_test.py` — 32 passed.
- No Cargo, release build, live notification, production change, or native
  gate execution was performed, per review scope.
