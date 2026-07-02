# Agent

## ADDED Requirements

### Requirement: superzej registers its MCP surface into installed external agents idempotently

superzej SHALL detect installed external agent CLIs and register its MCP surface
into each agent's configuration, preferring the agent's own registration CLI and
falling back to an idempotent, non-clobbering merge of the agent's config file
that inserts or updates only superzej's server entry. Re-running registration MUST
be a no-op, unrelated config entries MUST be preserved, and an unregister action
MUST remove only superzej's entry. When no external agents are present, detection
MUST no-op without error.

#### Scenario: Registration adds superzej without clobbering

- **WHEN** superzej registers into an agent config that already has other MCP
  servers
- **THEN** superzej's server entry is added and the existing entries are unchanged

#### Scenario: Re-registration is idempotent

- **WHEN** registration runs again for an already-registered agent
- **THEN** the config is unchanged (no duplicate entry)

#### Scenario: Unregister removes only superzej

- **WHEN** the user unregisters superzej from an agent
- **THEN** only superzej's entry is removed and other servers remain

#### Scenario: No external agents present

- **WHEN** no external agent is installed
- **THEN** detection completes with nothing registered and no error

### Requirement: Agent-facing errors carry stable machine-readable markers

superzej SHALL emit stable, machine-readable markers in agent-facing error text at
the approval (bouncer) and quota/route-failure (proxy) seams, each paired with a
next-step instruction, so an agent can act on the failure rather than treating it
as opaque. An unrecognized condition MUST fall back to a generic marker.

#### Scenario: Approval-required error is actionable

- **WHEN** an agent action is blocked pending human approval
- **THEN** the error text includes a stable approval-required marker with a
  next-step instruction

#### Scenario: Quota-exhausted error is actionable

- **WHEN** the proxy refuses a request because quota is exhausted
- **THEN** the error text includes a stable quota-exhausted marker with a next-step
  instruction
