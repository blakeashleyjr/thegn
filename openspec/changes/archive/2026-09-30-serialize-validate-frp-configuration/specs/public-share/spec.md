## ADDED Requirements

### Requirement: Validate FRP plans before secret resolution and serialization

The existing FRP share planner SHALL validate server address, worktree label, local port, applicable subdomain and subdomain host before resolving token references or serializing configuration. It SHALL reject nonempty raw `extra` entries and zero TCP/UDP remote ports. Diagnostic errors SHALL identify the invalid field without copying its untrusted value or resolved token.

#### Scenario: Invalid web endpoint has no credential side effect

- **WHEN** a web share has an invalid subdomain host or derived DNS label
- **THEN** planning fails before the token resolver runs and before a configuration file or client process is produced

#### Scenario: Invalid fixed port or raw extension is refused

- **WHEN** the local port is zero, a TCP/UDP remote port is zero, or raw extra fields are supplied
- **THEN** planning returns an explicit validation error and produces no launch plan

### Requirement: Serialize a strict typed FRP document

The planner SHALL serialize server, optional token authentication and one proxy through typed TOML serialization, reparse into the same unknown-field-rejecting types and compare all protected fields before returning the plan. Generated configuration contents SHALL be redacted from plan debug output.

#### Scenario: Token contains TOML syntax and Unicode

- **WHEN** a resolved token contains quotes, backslashes, control characters or Unicode
- **THEN** the reparsed authentication token equals the resolved input and cannot introduce another key or table
- **AND** debug output omits the generated configuration contents

#### Scenario: Existing protocol and endpoint forms remain usable

- **WHEN** a valid HTTP, HTTPS, TCP or UDP configuration is planned
- **THEN** its existing proxy type and applicable port/subdomain fields are retained
- **AND** IPv6 is unbracketed in the FRP server field and bracketed in a derived host-port URL
- **AND** a zero configured virtual-host port retains the existing omitted URL suffix behavior
- **AND** a 63-byte worktree label remains accepted for a TCP proxy name
