# Merge Queue

## ADDED Requirements

### Requirement: Sealed merge gate

When `[merge_queue] gate_sandbox = "sealed"` (global or trusted-profile config
only), thegn SHALL run `gate_setup_command`, `gate_command` and
`regenerate_command` inside the repo's sealed sandbox, with network none, an
allowlisted environment, and only the gate checkout and its target dir
writable. thegn MUST NOT run any of them on the host in that mode.

#### Scenario: Sandbox cannot start

- **WHEN** the sealed sandbox is refused or fails to start
- **THEN** the gate verdict is an infrastructure error and the land asks for approval

#### Scenario: Gate needs the network

- **WHEN** a sealed gate needs a dependency download
- **THEN** the gate is red, its log starts "sandboxed gate (network: none)", and it never lands

### Requirement: Unattended drain and the fixer

An unattended drain SHALL NOT hand a red gate or a conflict to the fixer agent.
