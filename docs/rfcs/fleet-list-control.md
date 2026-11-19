# `fleet.list` control and MCP design

Design note · 19 November 2026 · THE-2020 remainder

## Goal

Make the existing fleet inventory a read-only control capability. A supervisor,
phone client, or MCP caller should be able to ask the daemon for the same
provider inventory and worktree join that `thegn fleet list --json` already
renders. The result must be bounded, redacted, cacheable, and safe to call
without a TUI or a foreground CLI process.

This is a design for one capability, not a second fleet implementation. The
control path reuses the existing `FleetView`/`FleetCost` model and the provider
inventory seams, with the daemon as the owner of the shared cache.

## Non-goals

- Starting, stopping, destroying, creating, resuming, or adopting a sandbox.
- Returning provider usage or billing records. Cost remains the CLI verb's
  responsibility; `fleet.list` returns the existing estimated `FleetCost` only.
- Resuming a stopped provider resource as a side effect of reading it.
- Adding a new provider, plugin API, gRPC proto message, config key, or help
  page.
- Replacing the TUI's fleet snapshot or changing the output of
  `thegn fleet list --json`.

## Capability row

The catalog is the source of truth described by
[ARCHITECTURE.md §6](../ARCHITECTURE.md#6-external-doors-one-capability-catalog).
The proposed id is new: `fleet.list` — **UNVERIFIED on BASE** (no such id is
present in the BASE catalog). A dedicated read verb should resolve through
`required_scope(verb)` to `Scope::Read`.

| Capability | Required scope | Declared surfaces | Projection |
| --- | --- | --- | --- |
| `fleet.list` | `Scope::Read` via `required_scope` | HTTP, gRPC, CLI, MCP, plugin | Cached view and estimate |

The initial surface plan is:

- HTTP: `GET /v1/fleet`, routed through the control server and the generic
  `thegn api call` spine.
- CLI: `thegn api call fleet.list` (the generic control client, not a new CLI
  verb).
- MCP state: the catalog id projects to the `fleet_list` tool through
  `StateRouter`.
- gRPC: `SURFACE_GAPS` entry — **UNVERIFIED on BASE for this pair**; the
  control gRPC mirror needs a proto message and handler addition.
- plugin: `SURFACE_GAPS` entry — **UNVERIFIED on BASE for this pair**; the
  plugin `host.call` projection needs an explicit fleet wire/redaction review
  before exposing provider inventory to plugins.

The gRPC and plugin entries are temporary implementation debt. They do not
mean that those doors are permanently forbidden; the catalog row remains
declared on both surfaces so the gap ratchet records the missing work. The
reasons follow the surface model in ARCHITECTURE.md §6: a missing projection is
an explicit `SURFACE_GAPS` row, while a capability that is deliberately never
exposed narrows its declared surface set instead.

## Wire contract

The response preserves the JSON shape already emitted by the private
`FleetReport` wrapper at
[`crates/thegn-host/src/cmd/fleet.rs:76`](../../crates/thegn-host/src/cmd/fleet.rs:76).
The `FleetView` and `FleetCost` members are the canonical payloads; they are
already serializable and are the types at
`crates/thegn-core/src/placement/fleet_cost.rs:301` and `:441`.

`fleet.list` returns this list-only envelope:

| Field | Type | Meaning |
| --- | --- | --- |
| `generated_ms` | integer | Time at which this estimate was produced |
| `view` | `FleetView` JSON | Rows, joins, partial, missing, down, orphan, and unrecorded state |
| `cost` | `FleetCost` JSON | Estimates, projections, unpriced, and budget fields |
| `agents` | object | Existing optional dispatch summaries |

The list response omits `vercel_usage` and `sprites_usage`. Those are explicit
`fleet cost` usage calls and are not part of this capability. Existing CLI JSON
must remain byte-compatible for the fields it already emits. The control schema
adds the corresponding `FleetView`, `FleetCost`, and response definitions to
[`docs/api/control-v1.json`](../api/control-v1.json), retaining control API
version `1` and making additive fields defaultable where the current Rust
types require it.

The wire schema must describe, at minimum, the current fields of `FleetView`:
`rows`, `orphans`, `missing`, `down`, `unrecorded`, and `partial`; and of
`FleetCost`: `per_row`, `per_provider_hourly`, `total_hourly`, `today`, `mtd`,
`projected_24h`, `projected_30d`, `unpriced`, `price_table_date`, and `budget`.
The nested row, rate, partial-environment, recorded-sandbox, and budget types
follow the serde JSON emitted by `fleet_cost.rs`; no control-only duplicate
model should be introduced.

## Execution and bounds

The daemon's control server owns the operation. The HTTP handler, generic CLI
call, and MCP state handler all invoke the same host-side control operation;
none of them calls provider inventory directly. The host handler runs the
blocking fleet read off the event loop, using the existing provider blocking
boundary and returning a cloned snapshot to the transport.

The daemon keeps one shared fleet cache for all of these callers. A successful
or explicitly partial read is reusable for 30 seconds. The cache key includes
the daemon instance and the effective fleet configuration; a cache miss or
expiry starts one refresh, and concurrent callers await that refresh rather
than each starting provider requests. Errors are returned to the caller and do
not turn into an unbounded retry loop.

Each provider inventory is independently bounded by
`crate::vps::INVENTORY_BUDGET` (120 seconds on BASE), including pagination and
provider response processing. A provider that exceeds its budget contributes a
partial/unavailable entry according to the existing `Inventory` semantics; it
does not hold up unrelated providers beyond the shared request deadline.

The read path has these hard behavior bounds:

- It never makes usage calls. In particular, it does not call the Vercel or
  Sprites usage endpoints; those remain in `fleet cost`.
- It never resumes a provider resource. The provider conformance case
  `reads_never_resume` is a required regression for every provider used here.
- It does not infer ownership from a provider-wide row. Existing worktree and
  Sprites-ledger ownership rules remain authoritative.
- It returns a partial view rather than treating an incomplete inventory as a
  complete absence list.
- Every provider-originated string is passed through the fleet sanitizer before
  it enters the response or an error. BASE's sanitizer is `sanitize_text` in
  `crates/thegn-host/src/cmd/fleet.rs:129`; the implementation should extract
  or reuse that behavior at the shared response boundary so the control path
  cannot bypass it.

## Surface and schema ratchets

The implementation is complete only when each existing projection and its
ratchet moves together.

| Ratchet | Required change |
| --- | --- |
| `api_calls_mirror_routes` | Add the HTTP method/path row beside the route. |
| `coverage_problems` | Add the row and verify all five surface ledgers. |
| `test/surface-gaps-ratchet.txt` | Pin the two temporary gRPC/plugin gap reasons. |
| API snapshots | Update `docs/api/control-v1.json` and its test snapshot. |
| MCP tool list | Add `fleet.list` to `MCP_STATE_CAPS`, `STATE_TOOLS`, and list assertions. |

The HTTP route and API-call row must be one response shape, and MCP must use
the same catalog id even though its tool-name projection is `fleet_list`.

## One-lane implementation file list

The following is the smallest coherent implementation slice. It keeps policy in
core, transport projection in the service, and provider work in the host.

| File | Change |
| --- | --- |
| `crates/thegn-core/src/control.rs` | Add a fleet-list verb and map it to `Scope::Read`. |
| `crates/thegn-core/src/capability.rs` | Add the row and two temporary surface gaps. |
| `crates/thegn-svc/src/control/routes.rs` | Add `GET /v1/fleet`, `API_CALLS`, and assertions. |
| `crates/thegn-host/src/daemon/service.rs` | Implement the cached host `ControlApi` handler off-loop. |
| `crates/thegn-host/src/daemon/mod.rs` | Wire the shared cache if construction needs it. |
| `crates/thegn-core/src/mcp/state.rs` | Add `STATE_TOOLS`, `MCP_STATE_CAPS`, and dispatch. |
| `crates/thegn-host/src/cmd/fleet.rs` | Share sanitization without changing CLI JSON. |
| `crates/thegn-host/src/fleet_read.rs` | Share the read and serialization path. |
| `docs/api/control-v1.json` | Version the response and nested JSON definitions. |
| `test/surface-gaps-ratchet.txt` | Pin only the two temporary surface gaps until gRPC and plugin land. |

No help-page edit is included; the help-page decision belongs to lane 18.

## BASE verification

The following checks were run with `git grep` against BASE (`f08a9aa5`). A
proposed symbol that does not exist yet is called out rather than presented as
an existing implementation fact.

| Reference | BASE result |
| --- | --- |
| `thegn_core::capability::CATALOG` | Found in core and control projections. |
| `required_scope` and `Scope::Read` | Found in core; read verbs resolve to `Scope::Read`. |
| `FleetReport` at host `cmd/fleet.rs:76` | Found; private wrapper for `generated_ms`, `view`, `cost`, usage, and `agents`. |
| `FleetView` / `FleetCost` | Found in `crates/thegn-core/src/placement/fleet_cost.rs`. |
| `crate::vps::INVENTORY_BUDGET` | Found; declared at `crates/thegn-svc/src/vps/mod.rs:46`. |
| `reads_never_resume` | Found in provider conformance and the Sprites conformance test. |
| `api_calls_mirror_routes` | Found in `crates/thegn-svc/src/control/routes.rs:760`. |
| `coverage_problems` | Found in `crates/thegn-core/src/capability.rs:1513`. |
| `test/surface-gaps-ratchet.txt` | Found and pinned by the capability tests and ARCHITECTURE.md §6. |
| `mcp::state::StateRouter` / `MCP_STATE_CAPS` | Found in `crates/thegn-core/src/mcp/state.rs` and the MCP command. |
| `sanitize_text` | Found at `cmd/fleet.rs:129`; private, so it must be shared or extracted. |
| `fleet.list` | **UNVERIFIED on BASE**; this RFC proposes the id. |
| `FleetList` / a fleet control handler | **UNVERIFIED on BASE**; no dedicated verb or handler exists yet. |

## Unverified

- The exact final HTTP path, request envelope, and whether `agents` belongs in
  the first control response are design choices for implementation review.
- The gRPC proto names and the plugin host-call wire mapping do not exist on
  BASE; their `SURFACE_GAPS` reasons above are provisional until the owners
  choose the projection.
- The final cache owner and invalidation hook in `DaemonService` are not yet
  implemented.
- The control schema snapshot and MCP tool list have not been regenerated.
- No runtime test was run in this docs-only lane.
