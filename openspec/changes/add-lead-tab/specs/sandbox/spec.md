# Sandbox

## ADDED Requirements

### Requirement: The Lead sandbox fails closed and hides the daemon

The Lead SHALL launch only inside a sandbox that masks the daemon socket and run
directories. If the sandbox is unavailable the launch MUST be refused and SHALL
NOT fall back to a host shell.

#### Scenario: No bwrap

- **WHEN** bwrap is missing
- **THEN** opening the Lead reports an error and starts no process

#### Scenario: Daemon socket probe

- **WHEN** the Lead's shell tries to connect to the daemon socket
- **THEN** the path is not present inside the sandbox
