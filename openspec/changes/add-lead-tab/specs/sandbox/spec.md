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

### Requirement: The admitted Lead cannot reach host-local services

The Lead SHALL run in an isolated network namespace. The launch MUST forward
only the scoped Lead listener to the host loopback, and MUST refuse the legacy
shared-host network setting.

#### Scenario: Isolated network admits only the broker path

- **WHEN** a Lead is admitted with the isolated network policy
- **THEN** host loopback services and abstract unix sockets are unreachable
- **AND** the scoped Lead listener remains reachable

#### Scenario: Shared host network is configured

- **WHEN** `[lead] network = "host"` is configured
- **THEN** the Lead launch is refused before creating sandbox state
