# THE-34 architect design — control-surface coverage learned from Herdr

## Decision summary

Ship three small, serial chunks:

1. Add a closed, machine-readable error code beside the existing HTTP error
   message and regenerate the control schema snapshot.
2. Add bounded, per-connection event filters and opt-in lag signaling to the
   existing WS/SSE/gRPC feed. Reuse the daemon's existing bounded broadcast;
   do not add a journal, timer, poll, or wake source.
3. Project `events.subscribe` through `thegn events tail`, with the same filter
   vocabulary and the same catalog row used by every other surface. Classify
   its value-taking completion slots immediately.

Chunks 1 → 2 → 3 run serially. Chunk 2 builds on the error envelope and
control-schema machinery from chunk 1; chunk 3 consumes the typed subscription
client from chunk 2. The chunks deliberately do not parallelize overlapping
transport, schema, or CLI files.

The comparison source is the issue body and the checked-in OpenSpec draft. The
worker does not fetch the Herdr URL. The useful lessons are typed narrowing
subscriptions, visible loss, and stable error codes. Herdr's single JSON-RPC
socket, metadata writes, atomic prompt operation, and replay are not adopted:
the first would create a second control door, the latter operations need new
catalog capabilities and policy, and this feed is intentionally ephemeral.

## Live-branch evidence and gap matrix

The catalog is authoritative. `Surface`, `SurfaceSet`, `HostCapability`, and
the catalog are in `crates/thegn-core/src/capability.rs:24-180,183-455`; the
`stub` field makes routed-but-inert work explicit. `SURFACE_GAPS` is an
allowlisted temporary-debt table at
`crates/thegn-core/src/capability.rs:709-1195`, pinned by
`test/surface-gaps-ratchet.txt:1-118` and set-equality tested at
`crates/thegn-core/src/capability.rs:1510-1559`. The current audit has 86
gap cells: gRPC 33, CLI 4, HTTP 21, MCP 28, and no plugin gaps. Those residual
families are real follow-up work; this issue must not erase them by changing
the catalog dishonestly.

| Catalog row / concern                      | Existing projection evidence                                                                                                                                                                                                                                                                                                                                                                           | Finding and design response                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `events.subscribe`                         | Catalog row `crates/thegn-core/src/capability.rs:362-368`; HTTP routes `/v1/events` and `/v1/events/sse` at `crates/thegn-svc/src/control/routes.rs:66-69`, with the API call row at `:161`; gRPC table at `crates/thegn-svc/src/control/grpc.rs:746-778` (events at `:773`); plugin bridge projection at `crates/thegn-host/src/cmd/api.rs:66-78` and `crates/thegn-core/src/plugin_api.rs:1422-1450` | HTTP, gRPC, and plugin are implemented. The catalog omits CLI, and both CLI ledgers (`crates/thegn-host/src/cmd/session.rs:1519-1594` and `crates/thegn-host/src/cmd/api.rs:92-108`) omit it. Add the CLI projection and no `SURFACE_GAPS` row.                                                                                                                                                                                                                          |
| HTTP/gRPC/CLI catalog projection mechanism | HTTP route/API tables and coverage tests at `crates/thegn-svc/src/control/routes.rs:29-201,293-331`; CLI ledger at `crates/thegn-host/src/cmd/session.rs:1519-1605`; runtime ledger at `crates/thegn-host/src/cmd/api.rs:66-108`; MCP table at `crates/thegn-core/src/mcp/state.rs:347-365,571-590`                                                                                                    | The one-catalog rule is already landed. `thegn api list/schema/coverage/call` is generic for routed non-streaming rows; do not add a Herdr-style parallel RPC registry.                                                                                                                                                                                                                                                                                                  |
| Residual rows                              | Representative pinned rows: gRPC `launch.preset`/`mcp_proxy.*` at `crates/thegn-core/src/capability.rs:732-748`, HTTP `launch.preset` and `doctor.bundle` at `:749-765`, HTTP secrets/projects/search at `:767-904`, MCP and orchestration rows at `:911-1084`, containers/model-proxy rows at `:1091-1194`                                                                                            | These are not all cheap feed work. Preserve the exact ratchet and its shrink-only test. Only the missing `events.subscribe` CLI cell is paid here.                                                                                                                                                                                                                                                                                                                       |
| Error body consistency                     | HTTP collapses errors to `{"error": message}` in `crates/thegn-svc/src/control/http.rs:105-107,164-175`; ad-hoc auth/bad-request paths are at `:217-230,435,726,730`; `ControlError` taxonomy is `crates/thegn-svc/src/control/mod.rs:505-542`; the client currently parses only `value["error"]` at `crates/thegn-svc/src/control/client.rs:53-141`                                                   | Add a closed `ControlErrorCode` and `ErrorBody { error, code }`. Preserve the message and HTTP status. Map `ControlError` once; label adapter-only auth/parse failures explicitly. gRPC keeps canonical transport statuses (`crates/thegn-svc/src/control/grpc.rs:53-101`), MCP keeps JSON-RPC errors, and plugin keeps its established `RpcErrorCode` (`crates/thegn-core/src/plugin_api.rs:1040-1087`): these are transport projections, not competing HTTP envelopes. |
| Schema/versioning                          | Snapshot generator and exact-match test are `crates/thegn-svc/tests/control_schema.rs:16-85`; committed schema is `docs/api/control-v1.json:1-end`; binary `PROTO_VERSION` is `crates/thegn-core/src/control_wire.rs:23`, and unknown tags are fatal in `:304-405`                                                                                                                                     | Error code and filter request fields are additive and must regenerate the snapshot in the same implementation chunks. Keep protocol version 1. Do not send a new binary event tag to an old decoder; the event primitive uses existing frame kinds plus a negotiated/opt-in lag signal, or an additive proto field where required.                                                                                                                                       |
| Auth and scope                             | HTTP policy is documented in `crates/thegn-svc/src/control/http.rs:1-10`; local-admin/bearer decisions are `:193-245`; event handlers authenticate `Verb::Events` at `:1406-1469`; scope policy is centralized at `crates/thegn-core/src/control.rs:489-570`                                                                                                                                           | Filters narrow an already authorized `events.subscribe` stream and never grant data. Unix peers retain implicit same-user/admin behavior; TCP WS/SSE/gRPC continue to require bearer auth. No new token, scope, or unauthenticated route.                                                                                                                                                                                                                                |
| Session-input interlock                    | MCP state capability admission is `crates/thegn-host/src/cmd/mcp.rs:374-403`; `sessions.input` specifically requires `allow_session_input` at `:390-401`, passed through serve/run at `:207-210,258-263,300-307`; user-facing explanation is `docs/help/cli.md:185-193`                                                                                                                                | Keep this interlock. An event tail is read-only and cannot enable input. Do not weaken or reuse `--allow-session-input` as a feed flag.                                                                                                                                                                                                                                                                                                                                  |
| Event transport/backpressure               | HTTP WS/SSE pumps are `crates/thegn-svc/src/control/http.rs:1406-1469`; gRPC pump is `crates/thegn-svc/src/control/grpc.rs:629-659`; source is `DaemonService::emit` at `crates/thegn-host/src/daemon/service.rs:313-315` and existing broadcast capacity 1024 at `crates/thegn-host/src/daemon/mod.rs:278`; client transport buffer is 256 at `crates/thegn-svc/src/control/client.rs:497`            | Filter in each connection pump. Preserve bounded buffers. Opt-in clients receive a visible lag marker/count; legacy clients retain silent skip semantics. Never block the daemon/PTY producer and never add a timer or polling wake source.                                                                                                                                                                                                                              |
| Help/config/ratchets                       | Help registration and ratchets are in `crates/thegn-host/src/help/pages.rs` and `crates/thegn-host/src/help/ratchet_tests.rs`; completion drift logic is `crates/thegn-host/src/complete.rs:496-571`; current completion ratchet is `test/completion-slot-ratchet.txt:1-35`; CORS is already documented in `config/config.toml.example:3707-3719`                                                      | Add CLI/help/API documentation. Add the two new CLI value slots to `thegn_core::completion::CATALOG`; because the completion ratchet only shrinks and rejects additions, it remains unchanged after classification. No config key is introduced, so no config example change is required. Run help, surface, completion, and schema ratchets in the owning chunk.                                                                                                        |

## Target design

### Error contract

Create a substrate-free closed enum in a new core sibling module (rather than
growing a transport god file). It has stable serialized ids for
`not_found`, `no_scope`, `conflict`, `unimplemented`, `internal`,
`unauthorized`, and `bad_request`. `thegn-svc::control::ControlError::code()`
maps its existing variants to the enum. HTTP uses one `ErrorBody` serializer
for `ControlError` and adapter validation/auth failures. The error string is
unchanged for compatibility; clients may switch on `code`, never prose.

The typed client retains status/message and adds a code accessor, accepting old
servers that have no `code` field by treating it as `None`. gRPC status classes,
MCP JSON-RPC error objects, and plugin `RpcErrorCode` remain their established
wire contracts. Tests must prove the mapping is closed, HTTP bodies always have
both fields on covered error paths, and old HTTP bodies still parse.

### Event subscription primitive

Add a pure, bounded `FeedFilter` beside `EventFrame` in
`thegn-core::control_wire`. Its vocabulary is derived from one shared
`EventFrame::kind()` function used by HTTP JSON/SSE event names, filter parsing,
and CLI formatting. `kinds` is an optional comma-separated subset; `session`
only narrows session-keyed activity/lease/exit frames. Unknown kinds, empty
items, and overlong session ids are rejected as `bad_request`. A missing filter
means today's complete feed. The filter can only drop frames after auth.

Use an opt-in `signal_lag` request option. When a bounded broadcast receiver
returns `Lagged(n)`, that connection emits a visible lag marker carrying `n`
and continues; without the option it preserves the current silent skip. Do not
add a replay journal or a `State` frame in this change. The existing
`Sessions` event plus `sessions.list`/`worktrees.list` provide an explicit
re-list path, and a state payload carrying svc-owned `SessionInfo`/
`WorktreeInfo` into substrate-free core would violate the current layering
(`crates/thegn-svc/src/control/mod.rs:35-101`). A bounded, core-neutral
snapshot is a separate proposal if a real consumer proves the race matters.

Mirror the filter and lag option in the gRPC `EventsRequest` with additive
fields and an additive lag event message. Keep the current HTTP WS/SSE auth and
the current transport buffer sizes. The typed control client exposes a default
subscription and an options-bearing subscription so the plugin bridge and CLI
can share one implementation.

### CLI projection

Add a sibling `cmd/events.rs` with `events tail`; do not grow `main.rs` or
`cmd/session.rs` into another protocol implementation. It consumes the typed
control client, defaults to the local discovered Unix socket, supports
`--kinds`, `--session`, `--signal-lag`, and `--json`, and exits with a clear
no-daemon error. Human output and NDJSON use one frame formatter; no polling
loop is permitted. The catalog row gains `Surface::Cli`, and both CLI coverage
helpers derive/declare that projection. `api call` remains request/response and
continues to exclude WS streaming rows.

### Invariants and exclusions

- No DB migration, state DB invocation, render-path change, or new TUI wake
  source.
- No new config key. Do not add CORS/TLS/auth changes; those are already
  represented by the current `[serve]` policy and pairing flow.
- `thegn-core` owns only pure error/filter/wire logic and unit tests; no Tokio,
  Axum, tonic, socket, or vendor type crosses into it.
- Existing daemon broadcast channels remain the sole event source. The feed
  pumps wait on channels and degrade at the edge on lag/disconnect.
- Do not claim the 86 existing non-event gap cells are fixed. The surface
  ratchet must remain exact except for paying a row that is actually removed;
  this change adds no excuse.
- Do not run e2e or full-workspace gates. Any manual `thegn` invocation must
  set `XDG_STATE_HOME` to a fresh temporary directory and must not point at a
  live state DB.

## Implementation order and verification

Chunk 1 owns the stable error type and snapshot update. Chunk 2 owns the pure
feed primitive and transport mirrors, and regenerates the now-expanded
snapshot. Chunk 3 owns the CLI projection, help pages, and completion catalog.
Each coder must commit exactly the subject specified in its chunk file. After
each chunk, run only the listed scoped quick/nextest commands and `git diff
--check`; never `just test`, `just ci`, or an e2e smoke test.
