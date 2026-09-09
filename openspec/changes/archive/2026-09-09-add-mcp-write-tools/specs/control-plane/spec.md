# Control Plane

## MODIFIED Requirements

### Requirement: MCP serves scope-gated state tools

`thegn mcp serve` SHALL expose state tools beside the docs tools, gated by
`--scopes`: each tool maps to one catalog capability claimed on the MCP
surface and is listed/callable only when its `required_scope` is within the
requested set; live data comes from the daemon, with a cache fallback where
honest, and a clean error naming the daemon when live data is required but
unreachable. `MCP_STATE_CAPS` SHALL list exactly the implemented
capabilities. A tool MAY declare an argument schema (name, type, required);
when it does, `tools/list` MUST publish that schema as the tool's
`inputSchema`, and a `tools/call` whose arguments do not satisfy it MUST be
rejected with a JSON-RPC "Invalid params" error before any daemon call is
made. Mutating state tools (`required_scope` above `Read`) MUST log an
audit event naming the capability and a redacted view of the call's
arguments; a tool's argument redaction MUST replace any value that can carry
a secret (terminal input bytes, launch environment variables) with a
non-reversible size descriptor rather than omitting the field, so the audit
trail still shows _that_ a value was present and roughly how large it was.
`thegn mcp serve` MAY require an additional, tool-specific opt-in beyond
scope for a capability whose blast radius exceeds its scope tier's other
members; such an opt-in MUST be an explicit flag or config key checked
alongside (never instead of) the scope check, and MUST still deny both
listing and calling the tool when scope is granted but the opt-in is not.

#### Scenario: A scope-excluded tool is refused

- **WHEN** the server runs with `--scopes` that exclude a tool's required
  scope and a client calls it
- **THEN** the reply is a JSON-RPC error naming the missing scope, and
  `tools/list` did not advertise it

#### Scenario: Malformed arguments are rejected before the daemon is called

- **WHEN** a client calls a state tool with arguments that do not satisfy its
  declared schema (missing a required field, or a field of the wrong type)
- **THEN** the reply is a JSON-RPC "Invalid params" error and no daemon call
  occurs

#### Scenario: A mutating tool call is audited

- **WHEN** a client successfully calls a state tool whose required scope is
  above `Read`
- **THEN** an audit event is logged naming the capability, the call's
  redacted arguments, and the outcome, with any argument value that can
  carry a secret replaced by a size descriptor rather than its content

#### Scenario: An opted-in-but-under-scoped tool stays refused

- **WHEN** a tool requires both a scope and an additional opt-in, and the
  server is launched with the opt-in but not the scope
- **THEN** the tool is neither listed nor callable — the opt-in narrows what
  the granted scope covers, it never substitutes for the scope

#### Scenario: A scoped-but-not-opted-in tool stays refused

- **WHEN** a tool requires both a scope and an additional opt-in, and the
  server is launched with the scope but not the opt-in
- **THEN** the tool is neither listed nor callable
