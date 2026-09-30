## ADDED Requirements

### Requirement: Bound tunnel-client output while preserving URL discovery

Share startup SHALL bound retained output lines and queues, discard oversized
lines, retry interrupted reads, and preserve a valid unterminated URL at EOF.
Readers SHALL continue draining after startup returns. A full diagnostic queue
SHALL NOT discard the separate startup URL signal.

#### Scenario: Output flood precedes the address

- **WHEN** diagnostics fill their bounded queue before a valid URL arrives
- **THEN** URL discovery still succeeds and retained output remains bounded

#### Scenario: Address arrives as readers disconnect

- **WHEN** a valid final address is emitted without a newline at EOF
- **THEN** startup checks the address signal before reporting disconnection

#### Scenario: Provider writes after startup

- **WHEN** startup has returned its URL and the provider continues writing
- **THEN** readers continue consuming its pipes through EOF

### Requirement: Redact credential-bearing share diagnostics

Debug output SHALL omit share arguments, environment values, URL-rule values,
and public addresses. Startup errors SHALL omit raw provider output.

#### Scenario: Provider emits a secret before failing

- **WHEN** a client exits during startup after printing credential-bearing output
- **THEN** the returned diagnostic reports failure without copying that output

#### Scenario: Share plan contains a private ticket

- **WHEN** a plan or running share is formatted for diagnostics
- **THEN** its arguments, environment values, and public ticket are redacted
