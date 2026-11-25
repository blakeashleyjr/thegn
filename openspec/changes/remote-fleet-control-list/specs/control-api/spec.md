## ADDED Requirements

### Requirement: Fleet inventory is available through read-scoped control surfaces

The control API SHALL expose `GET /v1/fleet` under the `fleet.list` capability
with `read` scope. The MCP state surface SHALL expose the same report as
`fleet_list`. Both projections SHALL reuse the CLI fleet report shape, including
the authoritative provider inventory, worktree reconciliation, explicit
unknown prices, estimate provenance, and dispatch summaries.

#### Scenario: Read the fleet through the control API

- **WHEN** a caller with read scope requests `GET /v1/fleet`
- **THEN** the daemon returns the same list report fields as `fleet list --json`
- **AND** the read performs no provider usage, resume, provisioning, stop, or
  destroy operation

#### Scenario: Read the fleet through MCP

- **WHEN** an MCP caller with read scope invokes `fleet_list`
- **THEN** it receives the same report as the control API
- **AND** a caller without read scope is refused before the report is fetched

#### Scenario: Provider inventory does not block the runtime

- **WHEN** the control API or MCP fleet read refreshes provider inventory
- **THEN** provider I/O runs on a blocking worker
- **AND** provider-originated strings are sanitized before the response
