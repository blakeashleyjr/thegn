# Add a host capability (control API / MCP tool / plugin host verb)

Every external door projects `thegn_core::capability::CATALOG`
(`openspec/specs/capability-catalog`).

1. **Verb**: add a `Verb` variant in `crates/thegn-core/src/control.rs`, its
   scope in `required_scope`, and the variant in `Verb::ALL`.
2. **Catalog row** in `crates/thegn-core/src/capability.rs`: stable
   `<domain>.<action>` id, the verb, a one-line summary, and the surfaces it
   belongs on (`SurfaceSet::ALL`, `OPERATOR` for admin, `STREAMING` for
   attach-style feeds).
3. **`ControlApi` method** (`crates/thegn-svc/src/control/mod.rs`; default to
   `Unimplemented` if transport-only impls can't serve it) and its
   `DaemonService` implementation (off the render loop — DB on
   `spawn_blocking`).
4. **Surfaces**: an HTTP route in `control/routes.rs` (the router is built
   from the table), a gRPC method + `GRPC_CAPS` entry, the CLI verb +
   `CLI_CONTROL_CAPS`, the MCP tool + `MCP_STATE_CAPS`, the plugin verb +
   `PLUGIN_HOST_CALL_CAPS`. Anything you do not implement yet is a
   `SURFACE_GAPS` entry with a reason — and that list only shrinks.
5. **Scope test** in `control/tests.rs`: under-scoped calls are rejected
   before the API runs.
6. **Docs**: the route table in `docs/superpowers/specs/control-api.md`.

**Gates:** `every_verb_has_exactly_one_row`, `admin_caps_never_reach_mcp_or_plugin`,
`gaps_are_real_and_unique`, and the five per-surface `coverage_problems`
tests (missing route, stale gap, unknown id all fail).
