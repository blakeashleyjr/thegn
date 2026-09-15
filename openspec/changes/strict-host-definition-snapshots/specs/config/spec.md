## ADDED Requirements

### Requirement: Strict persisted host-definition capture

The core HostStore SHALL offer a distinct bounded host-definition capture
operation on an already-authorized connection. It MUST validate supported schema
version, ordinary main table shape and raw rows in one read transaction and MUST
refuse malformed or excessive source rather than return partial successful data.
Its result MUST NOT represent launch permission, runtime containment or freshness
after the capture. Existing display readers SHALL preserve their compatibility.

#### Scenario: Malformed persisted definition

- **WHEN** a config-bearing row has invalid JSON, duplicate keys, unknown fields,
  invalid enum values, wrong SQL types, invalid UTF-8 or invalid/duplicate names
- **THEN** strict capture returns a bounded typed error without source contents
- **AND** the row is not silently omitted or coerced into a valid default host

#### Scenario: Malformed definition shadowed by declarative configuration

- **WHEN** a malformed persisted definition has the same name as a valid declarative Local host
- **THEN** actual checked-store capture still returns the typed source error for invalid JSON, enum spelling or unknown fields
- **AND** it does not yield a successful snapshot or modify the declarative configuration or persisted row
- **AND** this source refusal does not represent a completed final-composition or launch-admission boundary

#### Scenario: Concurrent writer

- **WHEN** another connection updates schema version or host definitions after
  the capture transaction's first schema read
- **THEN** the capture retains one coherent version/schema/data snapshot
- **AND** a later capture independently rechecks version and source

#### Scenario: Resource bounds and nullable inventory

- **WHEN** host inventory or raw definitions exceed fixed count/byte/schema/JSON bounds
- **THEN** capture refuses explicitly without truncating the source
- **AND** NULL definitions count as inventory but do not become user host definitions

#### Scenario: Caller transaction and connection policy

- **WHEN** the connection already has an active transaction
- **THEN** strict capture refuses without committing or rolling back caller state
- **AND** capture never opens/migrates a DB or changes connection-wide policy
