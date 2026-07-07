# mcp-servers Specification

## ADDED Requirements

### Requirement: Advertised tools are filtered by agent identity

The MCP router SHALL filter the tools it advertises and dispatches by the calling
agent's gateway policy. At `tools/list` it MUST advertise only the tools the
identity's policy allows; at `tools/call` it MUST refuse a call to a tool outside
that allowed set with an MCP error and without producing a side effect. When the
identity has no gateway policy, the router MUST advertise and dispatch the full
house-tool set as before (transparent pass-through). The filtering decision MUST be
the pure, core gateway evaluator, so it holds identically for the in-process router
and the MCP-over-ACP path.

#### Scenario: tools/list is narrowed to the policy's allowed set

- **WHEN** an agent whose policy allows only `read_*` and `git_log` requests `tools/list`
- **THEN** the router advertises only the matching house tools and omits the rest

#### Scenario: tools/call denies an unlisted tool

- **WHEN** the same agent issues `tools/call` for a tool outside its allowed set
- **THEN** the router returns an MCP error and performs no side effect

#### Scenario: No policy advertises the full set

- **WHEN** an agent with no gateway policy requests `tools/list` and calls a tool
- **THEN** the full advertised house-tool set is listed and callable, unchanged
