# Chunk 3 — `preview.fetch` catalog and control projections

Commit subject (exact): `feat(the-13): expose preview.fetch capability`

## Files touched

- `crates/thegn-core/src/control.rs` — add read-scoped `PreviewFetch` verb and
  exhaustive scope/name mappings.
- `crates/thegn-core/src/capability.rs` — add exactly one `preview.fetch`
  catalog row, remove no existing row, and keep `browser.drive`'s stub marker.
- `crates/thegn-core/src/mcp/state.rs` — add the read-only MCP state-tool spec,
  argument schema, and `MCP_STATE_CAPS` entry.
- `crates/thegn-svc/src/control/mod.rs` — add bounded request/reply wire types
  and the object-safe `ControlApi::preview_fetch` method; no async trait syntax.
- `crates/thegn-svc/src/control/routes.rs` — add `/v1/preview/fetch` and its
  `API_CALLS` row.
- `crates/thegn-svc/src/control/http.rs` — authenticate with the catalog's
  `required_scope`, validate/decode the request, and serialize the reply.
- `crates/thegn-svc/src/control/grpc.rs` — mirror the method and include it in
  `GRPC_CAPS`.
- `crates/thegn-svc/proto/thegn/control/v1/control.proto` — add the request,
  bounded reply fields, and `PreviewFetch` RPC.
- `crates/thegn-svc/src/control/client.rs` — add the typed control-client call
  used by MCP and tests; generic `thegn api call` remains the primary CLI
  projection.
- `crates/thegn-svc/src/control/tests.rs` — update the recording fake and route
  coverage tests.
- `crates/thegn-svc/tests/control_schema.rs` — exercise the additive wire
  contract/snapshot update.
- `crates/thegn-host/src/preview_fetch.rs` — new host-owned HTTP executor using
  the existing `reqwest` dependency: GET-only, no cookies/auth/proxy, bounded
  timeout/body/redirects, and loopback validation through core policy.
- `crates/thegn-host/src/main.rs` — register `preview_fetch`; this is the
  intentional one-line serial overlap with chunk 2's host module registration.
- `crates/thegn-host/src/daemon/service.rs` — implement the control method by
  calling the host fetch executor and the chunk-2 diagnostic snapshot; surface
  precondition/limit/transport errors, never `Unimplemented`.
- `crates/thegn-host/src/cmd/mcp.rs` — route `preview.fetch` through the typed
  control client and return its bounded JSON result.
- `docs/api/control-v1.json` — regenerate with the repository's control-schema
  snapshot command; review the diff for only intended additive types/RPC wire.
- `test/surface-gaps-ratchet.txt` — remove no unrelated debt; verify no
  `preview.fetch` gap is introduced once all five projections are present.
- `test/completion-slot-ratchet.txt` — verify unchanged because the generic
  `api call --params` slot already exists; if a dedicated URL CLI is added,
  classify it in the completion catalog and remove its existing debt rather
  than adding a new ratchet line.

## Approach

Use the catalog as the only identity and the existing route/API-call spine as
the only generic projection. `preview.fetch` is read-only (`required_scope`
returns `Scope::Read`). HTTP and gRPC accept the same JSON/proto semantics;
the generic CLI invokes the route; MCP advertises `preview_fetch` only when its
read scope is enabled; plugin `host.call` becomes available through the
existing non-streaming route projection. Do not implement `browser.drive` as a
side effect of this chunk.

The request requires `url`; `include_console` only controls whether the
bounded, source-labelled pane diagnostics are included. The host executor
validates the URL before connecting, disables ambient credentials and proxies,
limits response bytes while streaming, caps redirects and validates every
redirect target, and returns status/content-type/body/truncation plus
diagnostics. It must not use the browser profile, cookie store, keychain, a
browser engine, or a shell command. Preserve server status codes as data where
possible and classify timeout/body-limit/loopback rejection as stable control
errors.

Add tests for loopback success, external rejection/explicit opt-in, redirect
escape rejection, timeout, body cap/truncation, cookie absence, diagnostics
redaction, scope rejection, HTTP route/API_CALLS mirroring, gRPC mapping, MCP
advertisement, plugin catalog projection, and control-schema determinism. Any
network fixture must bind a test-only loopback listener and use a temporary
`XDG_STATE_HOME`.

## Overlap/dependency

This chunk is file-disjoint from chunk 1 and chunk 2 at the implementation
level except for the intentional `crates/thegn-host/src/main.rs` module
registration overlap; it depends on both: chunk 1 supplies fetch policy and
chunk 2 supplies the live diagnostic snapshot. Run serially after them. It also touches the
shared control/catalog ratchet surfaces listed above; no other coder may edit
those files in parallel.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core capability`
- `cargo nextest run -p thegn-core mcp::state`
- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc control`
- `cargo nextest run -p thegn-svc control_schema`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host preview_fetch`
- `cargo nextest run -p thegn-host mcp`

Ratchet handoff: chunk 1 owns `test/env-overlay-ratchet.txt`; this chunk owns
the control-schema and completion/surface checks above and must leave the
environment ratchet unchanged except for a verified clean result.

For the schema test, use the normal snapshot command only with
`THEGN_UPDATE_SNAPSHOTS=1` after reviewing the additive diff. Use a temporary
`XDG_STATE_HOME` for every DB-owning test/helper. Do not run `just test`,
`just ci`, a full workspace compile, or E2E.

## Done criteria

- `preview.fetch` has exactly one catalog row, one read scope, HTTP/gRPC/CLI/
  MCP/plugin projections, and no surface-gap excuse; `browser.drive` remains
  explicitly a separate compatibility stub.
- The host fetcher is bounded, localhost-only by default, cookie/auth/proxy
  free, redirect-safe, and reports pane diagnostics honestly without claiming
  browser JavaScript-console access.
- `docs/api/control-v1.json`, control tests, catalog tests, and the owned
  completion/surface ratchets are synchronized in this commit; the env/help
  ratchets remain synchronized by chunks 1 and 2.
- The coder commits exactly with subject:
  `feat(the-13): expose preview.fetch capability`
