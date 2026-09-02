# Chunk 2 — provider bounds, control API, MCP/CLI projection, and refresh reads

## Files touched

- `crates/thegn-svc/src/ci.rs`
- `crates/thegn-svc/src/control/mod.rs`
- `crates/thegn-svc/src/control/routes.rs`
- `crates/thegn-svc/src/control/http.rs`
- `crates/thegn-svc/src/control/http_ci.rs` (new)
- `crates/thegn-svc/src/control/client.rs`
- `crates/thegn-svc/tests/control_schema.rs`
- `crates/thegn-host/src/daemon/service.rs`
- `crates/thegn-host/src/cmd/mcp.rs`
- `crates/thegn-host/src/cmd/ci.rs`
- `crates/thegn-core/src/completion/catalog.rs`
- `docs/api/control-v1.json`
- `test/completion-slot-ratchet.txt`

## Approach

1. In the existing forge implementation module, add a bounded child-output
   collector for log calls. Keep `gh`/`glab` invocation text inside their
   respective provider implementations; retain reserved providers and the
   object-safe seam. Never put a vendor binary in core/control/MCP.
2. Add read-only `ci_runs` and `ci_logs` control wire types/methods and thin
   HTTP routes (`/v1/ci/runs`, `/v1/ci/logs`) backed by the daemon’s cache/service
   projection. Put CI handler bodies in `http_ci.rs`; keep `http.rs` as a thin
   adapter and expose only the minimum `pub(super)` helpers needed to avoid
   god-file growth. Update route/API tables and the control schema snapshot in
   this chunk.
3. Implement daemon methods through the existing `with_db`/blocking boundary:
   cache-first, bounded provider-on-miss, redacted output only, and graceful
   cache/error responses. Never call provider code on the compositor thread.
4. Add the two MCP state-tool fetch branches using the chunk-1 parameterized
   router. Enforce the catalog-derived scope and return explicit stale/source/
   truncation/redaction metadata. Preserve the existing local DB fallback rules.
5. Make `thegn ci logs` the visible plural command while retaining `ci log` as
   an alias. Make both cache-first and preserve JSON stdout validity. Existing
   mutation commands remain provider-authoritative. Update the completion
   catalog and completion-slot ratchet in this chunk.

## Overlap and dependency

No file overlap with chunks 1 or 3. Run serially after chunk 1 because this chunk
consumes its `CacheStore` methods, control verbs/catalog IDs, wire shapes, and
MCP specs. Chunk 3 consumes the completed control/cache behavior but owns all
host refresh/UI/autofix files.

## Tests to run

- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc ci`
- `cargo nextest run -p thegn-svc control_schema`
- `THEGN_UPDATE_SNAPSHOTS=1 cargo nextest run -p thegn-svc control_schema` (only
  when the wire contract intentionally changed, then rerun without the env var)
- `just quick thegn-host`
- `cargo nextest run -p thegn-host cmd::ci`
- `cargo nextest run -p thegn-host mcp`

Do not run a full workspace build, `just test`, `just ci`, e2e, a migration, or
the built binary. Any manual `thegn` call must use a fresh `XDG_STATE_HOME`.

## Done criteria

- Provider log calls are bounded while draining and remain isolated to
  `thegn-svc/src/ci.rs`; reserved providers remain reserved.
- HTTP, CLI, daemon, and MCP all project the exact catalog capabilities and
  expose only bounded/redacted cache data with source/timestamp metadata.
- Control route/API tables and `docs/api/control-v1.json` pass the snapshot;
  no gRPC proto is changed and no surface-gap excuse is added.
- `ci logs` and compatibility `ci log` never put an error string on JSON
  stdout, and cache misses degrade clearly.
- Commit exactly as: `feat(the-48): expose cached CI logs through control surfaces`
