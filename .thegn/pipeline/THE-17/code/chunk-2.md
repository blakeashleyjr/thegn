# Chunk 2 — catalog, control API, and MCP projection

Commit subject (exact): `feat(the-17): expose editor handoff through control and MCP`

## Files touched

- `crates/thegn-core/src/capability.rs`
- `crates/thegn-core/src/control.rs`
- `crates/thegn-core/src/mcp/state.rs`
- `crates/thegn-svc/src/control/mod.rs`
- `crates/thegn-svc/src/control/routes.rs`
- `crates/thegn-svc/src/control/http.rs`
- `crates/thegn-svc/src/control/client.rs`
- `crates/thegn-svc/src/control/grpc.rs`
- `crates/thegn-svc/proto/thegn/control/v1/control.proto`
- `crates/thegn-svc/tests/control_schema.rs`
- `docs/api/control-v1.json` (regenerated additive snapshot)
- `crates/thegn-host/src/daemon/service.rs`
- `crates/thegn-host/src/cmd/mcp.rs`

## Approach

Add `Verb::OpenEditor`, one catalog row `editor.open`, and write scope. Use
the existing generic route spine at `POST /v1/editor/open`; add a wire request
with required `worktree` and optional relative `path`, `line`, and `col`.
Reject unknown fields and invalid target shapes at the request boundary, but
leave path containment and provider argv policy to the core target policy.
The result acknowledges that an intent was queued.

Add matching HTTP, gRPC, and `ControlClient` adapters. Add the MCP state tool
and `MCP_STATE_CAPS` row with the same argument schema and write-scope gating.
The generic CLI `thegn api call` gets coverage through `API_CALLS`; do not add
a second dedicated CLI command or a new completion slot. Keep the existing
`api call params` classification in `test/completion-slot-ratchet.txt` and
run its ratchet unchanged.

Implement `DaemonService::open_editor` as an intent mailbox write only. The
payload contains worktree, optional relative file, line, column, and source;
it never contains argv, executable, provider, or environment. Use the existing
best-effort mailbox pattern and do not add a migration. The compositor-side
claim/launch is chunk 3.

Add the route to `API_CALLS`, gRPC capability list, MCP state tool list, and
the control schema snapshot in the same chunk. The catalog’s surface coverage
must show HTTP, gRPC, CLI, MCP, and plugin projection as implemented. Do not
paper over missing work with `SURFACE_GAPS`; only remove an already-present
draft excuse if necessary.

## Dependencies and overlap

Serial after chunk 1 because the wire request and daemon payload use the core
target/request vocabulary. File-disjoint from chunk 1. Chunk 3 is serial after
this chunk because it consumes the new `ControlApi` method and intent carrier;
the daemon service file must not be edited by both coders. THE-27 has no file
overlap with the transport files, but the final branch integration still
requires rebasing chunk 3 after THE-27 if its run/model paths changed.

## Tests to run

- `just quick thegn-core`
- `just quick thegn-svc`
- `cargo nextest run -p thegn-core capability`
- `cargo nextest run -p thegn-core mcp::state`
- `cargo nextest run -p thegn-svc control`
- `cargo nextest run -p thegn-svc control_schema`
- `cargo nextest run -p thegn-host cli_control_caps`

Regenerate the snapshot only with:
`THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema`.
Run the focused surface-gap and completion ratchet checks available in this
checkout. Do not run `just ci`, `just test`, e2e, or a full workspace compile.

## Done criteria

- `editor.open` has exactly one catalog row, one verb/scope policy, and no
  unimplemented-surface excuse.
- HTTP, gRPC, generic CLI API calls, MCP, and plugin host-call projection all
  expose the same safe request; control schema JSON is committed and additive.
- MCP discovery and invocation are write-scope gated and use the same four
  arguments; audit behavior follows existing mutating calls.
- The daemon only queues a validated-shaped intent and never launches a child.
- Completion-slot, surface-gap, control-schema, and catalog tests are green
  with no unjustified ratchet growth.
- The coder commits exactly as:
  `feat(the-17): expose editor handoff through control and MCP`
