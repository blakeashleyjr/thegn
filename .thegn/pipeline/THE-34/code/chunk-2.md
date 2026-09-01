# THE-34 chunk 2 — filtered event feed with visible lag

## Scope

Add the event-subscription primitive over the existing bounded daemon broadcast:
per-connection narrowing filters and opt-in lag signaling, mirrored by WS,
SSE, gRPC, and the typed control client. This chunk deliberately cuts the
draft's `State { SessionInfo, WorktreeInfo }` snapshot: those response types
belong to `thegn-svc`, not substrate-free `thegn-core`, and the existing
re-list capabilities are the honest resync path until a bounded core-neutral
snapshot contract is separately designed.

## Exact files touched

- `crates/thegn-core/src/control_wire.rs` (bounded filter, shared kind names,
  additive lag representation, unit tests)
- `crates/thegn-svc/src/control/http.rs` (query parsing, per-pump filter,
  SSE event names, opt-in lag behavior)
- `crates/thegn-svc/src/control/grpc.rs` (request fields, filter mapping,
  additive event mapping, tests)
- `crates/thegn-svc/src/control/client.rs` (options-bearing subscription and
  shared JSON/frame formatter)
- `crates/thegn-svc/proto/thegn/control/v1/control.proto` (additive Events
  request/response fields)
- `crates/thegn-svc/tests/control_schema.rs` (include the public filter/request
  wire type if applicable)
- `docs/api/control-v1.json` (regenerated snapshot)
- `docs/superpowers/specs/control-api.md` (feed filter/lag contract)

No daemon source, database file, config file, plugin API file, or compositor
file should change: the daemon already emits on
`crates/thegn-host/src/daemon/service.rs:313-315` into the bounded channel at
`crates/thegn-host/src/daemon/mod.rs:278`.

## Approach

1. In `thegn-core::control_wire`, implement `FeedFilter` with a fixed frame
   vocabulary and a length-bounded session id. `EventFrame::kind()` is the one
   source for filter validation, HTTP JSON/SSE `event:`, and client formatting.
   Unknown/empty kinds are errors. Unit-test every frame family, session
   narrowing, absent filters, and bounds. Keep core free of Tokio/HTTP/tonic.
2. Add an explicit `signal_lag` option. On
   `broadcast::error::RecvError::Lagged(n)`, opted-in connections emit a
   visible lag marker with `n` and continue; default connections retain the
   current silent skip. Filter after receiving from the existing broadcast,
   never at the producer, and never add a timer or polling wake.
3. Parse WS/SSE query parameters before upgrade/stream creation. Invalid input
   returns the chunk-1 structured `bad_request`. SSE adds the shared kind as
   its `event:` field while preserving its data payload contract. gRPC gets
   additive `kinds`, `session`, and `signal_lag` request fields plus the
   corresponding additive event representation; invalid filters map to
   `invalid_argument`.
4. Extend the client with a default `subscribe_events()` compatibility helper
   and an options-bearing method. Keep the existing 256-frame client buffer and
   make the formatter usable by the CLI without duplicating event-kind policy.
5. Regenerate the schema snapshot in this same chunk. Update the control-plane
   spec to state that filters narrow only, loss is visible only when opted in,
   and the protocol remains v1. Add tests proving a parameterless subscriber
   sees the current frame set and new lag behavior is opt-in.

## Dependency/overlap

Serial after chunk 1. This chunk overlaps chunk 1 in `http.rs`, `client.rs`,
`control_schema.rs`, the committed schema, and the control-plane spec. Chunk 3
depends on this chunk's options-bearing client and formatter but is otherwise
file-disjoint.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core control_wire`
- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc control_events`
- `cargo nextest run -p thegn-svc control_grpc`
- `cargo nextest run -p thegn-svc control_schema`
- `git diff --check`

Use unit/transport-pump tests with injected bounded channels. Do not start a
live daemon, invoke against a real state DB, run e2e, or run full-workspace
gates. If a manual `thegn` invocation is added for diagnosis, use a fresh
temporary `XDG_STATE_HOME`.

## Done criteria

- WS, SSE, and gRPC accept the same bounded filter vocabulary and reject typos
  as structured bad requests/invalid arguments.
- Filtering occurs per connection and cannot broaden the authorized `read`
  stream. Existing auth and scope checks remain the only gate.
- Opted-in consumers receive a lag marker/count and continue; default consumers
  preserve legacy behavior. No new source, timer, poll, wake, DB, or buffer
  amplification is introduced.
- Frame-kind naming is shared across filter parsing, SSE metadata, JSON, and
  CLI-facing client formatting.
- The control schema snapshot is regenerated and exact-match tests pass.
- The draft `State` snapshot is explicitly not implemented in this chunk.
- Commit exactly as: `feat(the-34): filter and signal control event subscriptions`
