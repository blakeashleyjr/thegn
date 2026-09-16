# Session, isolation and terminal remediation review (2026-09-13)

Scope: THE-615, THE-616, THE-617, THE-618 under THE-614. Implemented in the
isolated audit/session checkout. No live processes, config, sessions or state
were changed. Original user edits remain untouched. This is a review checkpoint,
not a claim that the live build contains these fixes.

## Changes

- Lazy daemon adapters pin their first endpoint for attach, absence, PID and kill.
  Missing/malformed rosters remain unknown; cancelled history claims roll back.
- Sprites persistent IDs stay on the native provider route. Only HTTP 200 with a
  complete strict session list proves absence. HTTP errors, missing identities,
  malformed JSON and oversized bodies cannot authorize a new shell. API contract:
  https://sprites.dev/api/sprites/exec. Iroh has no persistent session IDs.
- Existing-ID recovery is bounded to six attempts, eight seconds per attempt and
  thirty seconds overall. Fresh open happens once only after proven absence.
  Existing bounded channels retain input ordering/backpressure, with one pending
  control held across transport failure. Committed controls are never replayed.
  Accepted resize dimensions survive reconnect and fresh fallback.
- An independent oneshot pane lifetime wakes blocked control/output forwarding.
  The relay remains the sole close-time kill owner; detach preserves the session.
  Fresh opens are not cancelled by the lifetime signal, avoiding a new ambiguous
  side-effect cancellation path. Recovery cancellation observes control ownership.
- Final actual host and bare SSH admission enforce the configured isolation floor,
  including no resolved candidate and preflight-failure fallthrough. Dormant
  recovery remains ahead of final host admission. Failed availability probes no
  longer claim that the runtime is stopped.
- Exact cached termwiz 0.23.3 is vendored with best-effort Unix/Windows destructor
  restoration. The only modified original upstream files are the two terminal
  implementations. Provenance/license retained; no dependency version drift.

## Verification

- Patched termwiz library Cargo build passed. Four private real-PTY tests passed:
  healthy mode restoration, hung-up cleanup, and primary unwind in a subprocess
  with exactly one child test executed. Source: vendored unix_drop_tests.rs;
  direct linked harness `/tmp/thegn-termwiz-patched-tests.rs`. Same tests are
  included in the host Unix test module for normal workspace CI.
- Eight actual-source relay/recovery harness tests passed (0.69 seconds), including
  transient recovery, strict absence, timeout, cancellation with buffered input,
  backoff cancellation, retained input exactly once, accepted resize persistence,
  and blocked-capacity close exactly once versus detach no kill. Harness:
  `/tmp/thegn-relay-fixture.rs`; production functions are extracted unchanged.
- Scoped thegn-core sandbox_floor tests: 10 passed.
- Scoped thegn-svc roster tests compiled: two pure decoder tests passed; the HTTP
  adapter test failed at TcpListener::bind with EPERM before serving any request.
  No tests were skipped to turn this into a passing result. Actual LazyDaemonSource
  socket adapter tests remain present and need an environment permitting listeners.
- Strict OpenSpec validation and source guards are recorded at checkpoint time.

## Pending integration validation

Root will combine branches before linking the host tests once. Required host
checks include the new agent floor tests, history claim rollback, daemon adapter,
relay tests and shared vendored PTY inclusion. The stronger-runtime-preflight-
failure path needs a supported isolated adapter fixture; pure floor tests do not
prove every launch path. HTTP endpoint, exact LazyDaemonSource and Windows runtime
checks remain pending. Standalone vendored Cargo tests cannot resolve uncached k9
in offline mode; the direct harness and host inclusion test the actual patch.
Full clippy/formatting/just ci and adversarial review remain integration gates.

The copied host build.rs is identical to the root branch build-watch fix; root
owns its evidence and merge. No release build was run by this lane.
