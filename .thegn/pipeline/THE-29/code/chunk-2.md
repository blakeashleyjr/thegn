# Chunk 2 — control API, wire, and surface projections

## Scope

Expose the chunk-1 contract through the service control API. This chunk is
file-disjoint from chunks 1 and 3 and runs after chunk 1 so generated schemas
and routes consume the settled types.

## Files touched

- `crates/thegn-core/src/control.rs`
- `crates/thegn-core/src/capability.rs`
- `crates/thegn-core/src/mcp/state.rs`
- `crates/thegn-core/src/completion/catalog.rs`
- `test/completion-slot-ratchet.txt`
- `test/env-overlay-ratchet.txt` (only if the ratchet changes; no new config
  key means no line should be added)
- `crates/thegn-svc/src/control/mod.rs`
- `crates/thegn-svc/src/control/routes.rs`
- `crates/thegn-svc/src/control/http.rs`
- `crates/thegn-svc/src/control/grpc.rs`
- `crates/thegn-svc/src/control/client.rs`
- `crates/thegn-svc/proto/thegn/control/v1/control.proto`
- `crates/thegn-svc/tests/control_schema.rs`
- `docs/api/control-v1.json`

## Approach

1. Add `Verb::ForkSession`, its catalog row, required scope, MCP state-tool
   descriptor/capability, and completion slots before wiring transports. Keep
   `sessions.fork` in the same catalog path as every other surface and do not
   add a `SURFACE_GAPS` excuse. Classify `session`, `worktree`, `agent`, and
   structural flags; a native id with no safe provider source is explicitly
   reserved/freeform.
2. Add additive `ForkSpec`/source representation, `SessionInfo.forked_from`,
   and `AgentLaunch` fork/native-id fields with serde defaults. Keep transport
   types free of argv/env recipe data for native sources. Add
   `ControlApi::fork` with the existing default-unimplemented behavior so
   non-daemon adapters degrade clearly until the host implements it.
3. Add one HTTP route and one `API_CALLS` row for
   `POST /v1/sessions/fork`, using the catalog scope gate. The request body is
   typed and source-discriminated; do not create a vendor-specific route or
   duplicate `agent.sessions` discovery. Ensure route/API mirror tests cover
   the new row.
4. Add the corresponding gRPC RPC/message and `info_to_proto` lineage field;
   derive the gRPC capability from the same catalog projection. Preserve
   non-streaming semantics and structured auth/audit errors.
5. Add typed client support used by CLI/MCP. Do not put worktree creation or
   PTY behavior in the service client.
6. Regenerate the control schema snapshot with the repository’s snapshot
   mechanism. Never hand-edit generated JSON. The snapshot, route mirror, wire
   round-trip, capability coverage, and control-schema tests are part of this
   same commit. The control-schema snapshot and completion-slot ratchet must
   be updated together here; check env-overlay coverage here too and do not add
   a config key.

## Overlap/dependency

No file overlaps chunk 1 or chunk 3. Chunk 2 depends on chunk 1’s harness,
policy, and cache naming. Chunk 3 depends on the exact `ForkSpec`, client
method, catalog, and proto generated types from this chunk. Run this chunk
after chunk 1 and before chunk 3; no chunk-2 file may be edited by chunk 3.

## Tests to run

- `just quick thegn-svc`
- `cargo nextest run -p thegn-core capability`
- `cargo nextest run -p thegn-core completion`
- `cargo nextest run -p thegn-svc control_schema`
- `cargo nextest run -p thegn-svc routes`
- `cargo nextest run -p thegn-svc control`
- `cargo nextest run -p thegn-svc grpc`

For the snapshot update, use the scoped repository command with an isolated
state environment if it invokes a binary:

```sh
XDG_STATE_HOME="$(mktemp -d)" THEGN_UPDATE_SNAPSHOTS=1 \
  cargo test -p thegn-svc --test control_schema
```

Do not run a live migration, `thegn` binary, full workspace build, `just test`,
`just ci`, or e2e.

## Done criteria

- `sessions.fork` is reachable through HTTP, gRPC, CLI generic control, MCP
  projection, and plugin generic routing from the one catalog, with the same
  write scope and no `SURFACE_GAPS` entry.
- HTTP `API_CALLS` mirrors `ROUTES`; gRPC capability and proto lineage fields
  are tested; default-unimplemented adapters return the normal clear error.
- `SessionInfo.forked_from` is additive/backward-compatible and no response
  leaks recipes, env, prompts, transcript bytes, or vendor file formats.
- `docs/api/control-v1.json` and control-schema snapshots are regenerated and
  committed in this chunk.
- Completion-slot and env-overlay ratchets are checked/updated in this same
  chunk; no new config key is introduced.
- Commit exactly as: `feat(the-29): expose sessions.fork across control surfaces`.
