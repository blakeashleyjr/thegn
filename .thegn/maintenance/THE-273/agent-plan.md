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

## Route classification (route -> budget)

Default 60s / 8 MiB: GET /health, /v1/me, /v1/sessions, /v1/leases, /v1/automations, /v1/mcp_proxy/status,
/v1/dispatches; POST /v1/dispatches, /v1/dispatches/{id}/status, /v1/notify, /v1/pair, /v1/worktrees/folder,
/v1/sessions/{id}/input|resize|detach|record. (DB or in-memory only.)
Long 300s / 8 MiB: GET /v1/worktrees (git), snapshot, /v1/pr/status (forge), /v1/issues\* GET/POST (tracker
network), DELETE /v1/sessions/{id} (reap), POST /v1/merge/add, /v1/automations/test, /v1/worktrees/open,
/v1/editor/open, /v1/mcp_proxy/reload, POST /v1/sessions, fork, split, tools run, POST /v1/worktrees, ci runs;
call_raw (generic) except /wait.
Long 300s / 64 MiB: ci_logs, preview_fetch.
Wait: caller timeout + 30s, or unbounded with no timeout.
Unsure routes were put in Long.
