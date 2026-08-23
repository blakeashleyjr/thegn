## Why

The capability catalog exists, but the client-API phase debt pinned in `SURFACE_GAPS` was still open: `pr.status` and `notify.push` were verbs without routes, `thegn mcp serve` exposed only docs tools (every `Surface::Mcp` claim excused), the control wire types had no published schema, and there was no generic, catalog-driven way for a script to call the control plane (`thegn api …`). External tooling had to hand-roll HTTP against undocumented shapes.

## What Changes

- **`pr.status` / `notify.push` become real**: `ControlApi::{pr_status, notify_push}` (wire types `PrStatusRow`, `PushedNote`), daemon implementations (DB-cache projection; notification-store insert), HTTP routes (`GET /v1/pr/status`, `POST /v1/notify`), gRPC mirrors, typed client methods; their route-lag `SURFACE_GAPS` entries burn.
- **MCP state tools**: `thegn_core::mcp::state::StateRouter` (pure, injected fetch closure, scope-gated) merged into `thegn mcp serve`, with `--scopes` selecting the allowed set; live data over the daemon control client with a DB fallback for worktrees; `MCP_STATE_CAPS` grows and the matching Mcp gap entries burn.
- **`thegn api list|schema|call`**: a catalog-driven CLI — `list` prints the catalog rows (surfaces, scopes); `schema` emits the control wire schema; `call <cap> [--params json]` resolves the verb's route from the `ROUTES` table and performs it over the control socket, JSON in/out — the generic dispatcher scripts (and later plugin host.call) build on.
- **Wire schema snapshot**: schemars on the control wire types, emitted to `docs/api/control-v1.json`, pinned by a snapshot test (`THEGN_UPDATE_SNAPSHOTS=1` regenerates) — the same discipline as the plugin wire.

## Impact

- Audit row C3. Specs: control-plane delta (+ capability-catalog delta for the burned gaps).
- Code: thegn-svc (control), thegn-host (daemon service, cmd/mcp.rs, new cmd/api.rs), thegn-core (mcp/state.rs, capability gap list), docs/api snapshot.
