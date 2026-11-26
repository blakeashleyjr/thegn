# MCP Servers

## ADDED Requirements

### Requirement: The Lead tools are served over MCP except approval resolution

`thegn mcp serve` SHALL expose the Lead capabilities as tools, and SHALL NOT
expose `approvals.resolve`. The Lead's server SHALL use a host-minted scoped
pairing token on a dedicated loopback listener that is not local-admin, started
only when `[lead]` is configured.

#### Scenario: No Lead configured

- **WHEN** `[lead]` is absent
- **THEN** no listener, thread or timer for the Lead exists

#### Scenario: Resolve is not a tool

- **WHEN** the Lead lists its MCP tools
- **THEN** no approval-resolving tool appears
