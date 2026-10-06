# THE-273 plan

Scope: crates/thegn-svc/src/control/client.rs only. No API shape / config key change.

- ControlLimits (connect 10s, request 60s, long 300s, body cap 8 MiB), typed ControlBoundError
  (ConnectTimeout / RequestTimeout / ResponseTooLarge); decode failures stay ControlProtocolError.
- Total deadline wraps every unary request; wait uses timeout_ms+30s grace, or explicitly unbounded
  when no caller timeout (daemon semantics: wait forever). call_raw derives /wait budget from body.
- Hyper path: bounded handshake, AbortOnDrop on conn task, capped collect. Reqwest: connect_timeout,
  Content-Length precheck, chunked cap.
- WebSocket connect+upgrade bounded by connect timeout; greeting already bounded (HELLO_TIMEOUT).
- Retries: none added, so THE-272 idempotency is not required.
  Tests: stalled peer, slow body, declared/chunked oversize on TCP+origin, stalled WS upgrade, wait budget.
