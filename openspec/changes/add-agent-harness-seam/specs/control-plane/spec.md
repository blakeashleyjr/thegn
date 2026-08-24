# Control Plane

## MODIFIED Requirements

### Requirement: MCP serves scope-gated state tools

`thegn mcp serve` SHALL expose state tools beside the docs tools, gated by a
scope set: each tool maps to one catalog capability claimed on the MCP surface
and is listed/callable only when its `required_scope` is within the granted
set; live data comes from the daemon, with a cache fallback where honest, and a
clean error naming the daemon when live data is required but unreachable.
`MCP_STATE_CAPS` SHALL list exactly the implemented capabilities. The granted
scope set SHALL resolve from config — the global `[mcp.serve] scopes`, narrowed
by the active profile's overlay, narrowed by the workspace overlay — with the
`--scopes` flag intersecting last; an inner level MUST only be able to narrow
the outer one, and an unparseable level MUST contribute nothing (fail-closed)
rather than widening. The effective set and the level that clamped it SHALL be
reported at server start and by `thegn doctor`.

#### Scenario: A scope-excluded tool is refused

- **WHEN** the server runs with an effective scope set that excludes a tool's
  required scope and a client calls it
- **THEN** the reply is a JSON-RPC error naming the missing scope, and
  `tools/list` did not advertise it

#### Scenario: A workspace overlay narrows the granted scopes

- **WHEN** global config grants read and write scopes but the workspace overlay
  lists only read
- **THEN** the server for that workspace serves read tools only, and reports
  the workspace overlay as the clamping level

#### Scenario: A repo-local overlay cannot widen the grant

- **WHEN** the workspace overlay lists a scope the global ceiling does not
  grant
- **THEN** the effective set stays within the global ceiling and the excess
  scope is ignored and reported, not honored
