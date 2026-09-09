# Delivery governance — cross-system truth delta

## ADDED Requirements

### Requirement: Active delivery work has one checked-in lifecycle identity

Every active delivery change SHALL map to a Linear issue and owning project, or
carry an explicit reviewed rationale for having neither. The checked-in mapping
SHALL distinguish proposed, active, delivered-awaiting-archive, and archived
states and SHALL be verifiable without network access.

#### Scenario: An active change has no issue or rationale

- **WHEN** the offline delivery-index gate finds an active change with neither a Linear identifier nor an explicit rationale
- **THEN** it fails and names the unmapped change

#### Scenario: An archived change is still called active

- **WHEN** a roadmap or delivery-index entry points to an archived change as in-flight
- **THEN** the gate fails and names both the stale reference and archive path

### Requirement: Live status is derived instead of copied into prose

Validation pass counts, plugin API versions and supported runtime extension
points, control scopes, generated wire schemas, and surface-gap counts SHALL be
derived from their authoritative machine-readable/code source or guarded by a
drift test. Documentation SHALL NOT claim a hard-coded live pass count.

#### Scenario: Plugin help lags the runtime API version

- **WHEN** the checked-in help example names a plugin API version different from `API_VERSION`
- **THEN** the documentation gate fails with both values

#### Scenario: OpenSpec validation changes item count

- **WHEN** strict validation succeeds with a different number of specs/changes than a prior run
- **THEN** the gate still succeeds because prose does not encode the transient count

### Requirement: External tracker reconciliation is read-only by default

The reconciliation report MAY augment local state with Linear status, project,
priority, and relationship data when credentials are available, but SHALL remain
useful offline and SHALL NOT mutate Linear. Issue closure and scope decisions
require the documented maintainer checklist.

#### Scenario: CI has no Linear credentials

- **WHEN** the reconciliation report runs in ordinary CI without external credentials
- **THEN** it validates and reports repository lifecycle truth without attempting a network call
