## ADDED Requirements

### Requirement: Host augmentation preserves effective definition precedence

When augmenting configuration with captured persisted host definitions, the
system SHALL retain a same-name declarative host and SHALL synthesize a missing
selectable environment from that effective winning host. Both placement and SSH
settings SHALL come from the same winner. An explicitly configured environment
SHALL remain unchanged by augmentation.

#### Scenario: Declarative local host shadows persisted SSH

- **WHEN** a declared local host and a persisted SSH host have the same name
- **THEN** the synthesized environment SHALL resolve locally and SHALL NOT inherit the persisted SSH destination

#### Scenario: Declarative SSH host shadows another persisted reach

- **WHEN** a declared SSH host shadows a same-name persisted host
- **THEN** its synthesized environment SHALL resolve with the declared destination, port, transport, forwarding and connection settings

#### Scenario: Winning reach has no pane transport

- **WHEN** the effective host reach is Iroh or cloud
- **THEN** augmentation SHALL NOT synthesize an SSH or local pane environment from a losing persisted definition
- **AND** selecting that absent environment SHALL retain the resolver's existing unresolved-selection diagnostic

#### Scenario: Explicit environment takes precedence

- **WHEN** an environment already exists for the augmented host name
- **THEN** augmentation SHALL preserve that environment and its existing resolution semantics

#### Scenario: Unshadowed persisted host

- **WHEN** no declarative host or explicit environment shadows a captured persisted host
- **THEN** supported SSH/local pane environments SHALL use that persisted definition's placement and connection settings
- **AND** Iroh/cloud definitions SHALL retain their existing absence of synthesized pane environments
