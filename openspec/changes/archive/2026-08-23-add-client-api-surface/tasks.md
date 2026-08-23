## 1. Control verbs

- [x] 1.1 `ControlApi::{pr_status, notify_push}` + wire types; daemon impls (DB projection; notification store)
- [x] 1.2 HTTP routes + gRPC mirrors + typed client methods; route-lag SURFACE_GAPS burned

## 2. MCP state tools

- [x] 2.1 `mcp::state::StateRouter` (pure, scope-gated, injected fetch) + serve merge + `--scopes`
- [x] 2.2 `MCP_STATE_CAPS` grown; Mcp gaps burned

## 3. Generic client + schema

- [x] 3.1 `thegn api list|schema|call` (ROUTES-driven dispatch over the control socket)
- [x] 3.2 schemars on control wire types → `docs/api/control-v1.json` + snapshot test

## 4. Gate

- [x] 4.1 clippy + control/mcp/api suites + `just lint`; openspec validate
