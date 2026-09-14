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
