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
   from the table) **plus its `API_CALLS` row** (`cap`, method, path —
   `api_calls_mirror_routes` fails without it; with it, `thegn api call`
   and the CLI surface (`cli_control_caps()` derives from the table) work
   with zero client code), a gRPC method + `GRPC_CAPS` entry, the MCP tool
   - `MCP_STATE_CAPS`. **Plugins need no per-verb code** — a non-streaming row
     listing `Surface::Plugin` is dispatched generically off the `API_CALLS`
     spine (`plugin_host_call_caps()` is derived from the catalog). Anything you
     do not implement yet is a `SURFACE_GAPS` entry with a reason **and a
     matching line in `test/surface-gaps-ratchet.txt`** — that allowlist is
     shrink-only (`ratchet_pins_surface_gaps`): adding an excuse fails until the
     file grows a line, burning one means deleting its line, and an empty table
     is pinned empty. A surface a capability is _deliberately never_ exposed on
     (e.g. admin verbs off MCP/plugin) is expressed by narrowing the row's
     surface set, never by an excuse. A routed-but-inert row marks itself with
     `stub: Some(reason)` so `thegn api coverage` counts it apart.
5. **Wire schema**: new wire types derive `schemars::JsonSchema`, join
   `tests/control_schema.rs`'s list, and the snapshot is regenerated
   (`THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test
control_schema`) — additive changes only within /v1.
6. **Scope test** in `control/tests.rs`: under-scoped calls are rejected
   before the API runs.
7. **Docs**: the route table in `docs/superpowers/specs/control-api.md`.

**Gates:** `every_verb_has_exactly_one_row`, `admin_caps_never_reach_mcp_or_plugin`,
`gaps_are_real_and_unique`, the five per-surface `coverage_problems`
tests (missing route, stale gap, unknown id all fail),
`api_calls_mirror_routes`, and the `control_schema` snapshot.
