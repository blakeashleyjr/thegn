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

### Requirement: Checked host composition before returning configuration data

The core SHALL offer an additive checked operation over caller-owned layered
Config and a strict HostDefinitionsSnapshot, plus a HostStore capture wrapper.
The wrapper MUST capture all source rows before composition. Composition MUST
use the existing effective winning-host merger and validate the admitted final
configuration with the existing project schema and semantic rules before
returning immutable configuration data. Legacy loaders SHALL remain unchanged.

#### Scenario: Effective winner and explicit environment

- **WHEN** valid persisted SSH shares a name with declarative Local or SSH
- **THEN** the final host and synthesized environment use the declarative winner
- **AND** an explicitly configured environment is preserved in full
- **AND** actual environment resolution retains the existing effective placement

#### Scenario: Invalid source or final configuration

- **WHEN** source capture fails, including a malformed shadowed row, or final schema/semantic validation fails
- **THEN** the operation returns a fixed typed error and no HostComposedConfig
- **AND** returned errors and successful result Debug expose no config contents
- **AND** no provider, config loader, migration or secret resolution is invoked

#### Scenario: Bounded structural and effective-profile work

- **WHEN** pre/post composition exceeds the checked byte, structural, stage, profile, rule or effective-profile work limits
- **THEN** it returns Bounds before semantic validation or effective profile cloning
- **AND** each nonempty profile charges the original base clone even when replacing its rules
- **AND** inclusive at-limit positive fixtures retain valid existing behavior

#### Scenario: Existing semantic compatibility

- **WHEN** undefined host references, disabled subsystems or non-pane reaches are otherwise accepted by existing validation
- **THEN** checked composition retains those existing semantics without inventing stricter global rules
- **AND** legacy validation keeps all diagnostic messages in their original order
- **AND** checked validation stops after its first failed batch before cloning later profiles

#### Scenario: Scope of successful data

- **WHEN** checked composition succeeds
- **THEN** its result represents existing final configuration validity only
- **AND** first schema initialization may construct environment-derived defaults for metadata without installing them into the supplied Config
- **AND** launch adapter, freshness, opening policy and runtime containment remain separate obligations
