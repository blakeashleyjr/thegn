# THE-682 native fixture fix

Verdict: implemented the reviewed test-only repair.

Diagnosis confirmed: `live.main()` now reaches `prebuild_preflight()`, whose
real `quiescent()` defaults to `/proc`; the two orchestration fixtures were
therefore inspecting the live machine and rejecting the running controller.
The dedicated process-inspection and preflight refusal tests already use
explicit private proc fixtures and were left unchanged.

Changes:

- Added a test-only context manager that creates an empty mode-0700 private
  proc directory and delegates to the real `live.quiescent()` implementation.
- Applied it only around the two `live.main()` orchestration fixtures.
- No production changes, weakened assertions, controller shutdown, or Rust
  build.

Verification:

- `python3 test/live_test.py` — 32 tests passed with the real thegn controller
  still present.
- `git diff --check` passed.
