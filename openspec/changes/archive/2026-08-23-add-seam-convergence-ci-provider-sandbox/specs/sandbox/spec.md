## ADDED Requirements

### Requirement: Backends are described by one profile table

Every runtime `Backend` SHALL have a `BackendProfile` (label, binary, family, rootful) and every per-backend decision (label, probe binary, OCI-ness, argv construction) MUST derive from it, keyed by `BackendFamily` rather than per-variant arms. `Backend::parse` MUST use the `config_enum!` alias table; a reserved config kind parses to no runtime backend.

#### Scenario: New OCI runtime

- **WHEN** a backend is added with `family: Oci`
- **THEN** the enter-argv builder and the container-removal loops serve it with no new match arm

#### Scenario: Reserved kind has no runtime

- **WHEN** `Backend::parse("wsl")` is called
- **THEN** it returns `None`
