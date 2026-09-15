## Implementation

- [x] Forward same-source daemon absence and add strict provider roster queries.
- [x] Retry exact session attachment with bounded time, pending controls and cancellation.
- [x] Enforce final host/remote isolation admission and correct probe diagnostics.
- [x] Vendor pinned termwiz with non-panicking cleanup and PTY regression tests.

## Verification

- [ ] Test missing/live/unreachable/malformed session results through real adapters.
- [x] Test retry success, exhaustion, input ownership, close and detach cancellation.
- [ ] Test actual host/remote launch admission for fail/degrade/off and provider bypass.
- [x] Run real PTY healthy, hangup and unwind cleanup tests.
- [ ] Run scoped crate tests/clippy, formatting, source ratchets and strict OpenSpec validation.
- [ ] Obtain root/adversarial code review and address findings before integration.
- [ ] Run `just ci` once at the final pre-PR gate, or record environment limitations.

Evidence and outstanding environment checks: `docs/audits/live-build-2026-09-13-session-remediation.md`.

## THE-617 preflight-failure follow-up — September 14

- [x] Add a private Linux fake-OCI fixture through actual launch preparation, proving stronger-candidate resolution, ensure success and exec-preflight failure before the final fail/degrade/off decision.
- [x] Preserve explicit-runtime refusal and independently review the exact argv allowlist, owned cleanup and diagnostic assertions.
- [x] Compile and execute the new fixture; source-bound combined05 native/strict-lint evidence is reused for the exact fixture and reviewed preparation path.
- [x] Pass standalone source ratchets, strict specification validation and formatting; complete root and independent adversarial review.
- [ ] Complete the reviewed standalone local-main landing.

The earlier full workspace receipt passed 8354/8354 tests with 26 configured skips; it predates this new regression and does not prove its execution. Current source-bound 8465/8465 receipt includes this regression; durable evidence is `docs/audits/the617-2026-09-15/manifest.json`. Exact scope: `docs/audits/THE-617-preflight-floor-acceptance-2026-09-14.md`. No real OCI isolation or new runtime support is claimed.
