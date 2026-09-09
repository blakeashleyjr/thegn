# delivery-governance Specification

## Purpose

TBD - created by archiving change enforce-delivery-state-drift-gates. Update Purpose after archive.

## Requirements

### Requirement: Active delivery work has bidirectional checked-in identity

Every active delivery change SHALL map to a Linear issue and owning project, or
carry an explicit reviewed rationale for having neither. Independently, every
reviewed active delivery issue SHALL be present in an issue-centric inventory
with its owning project and OpenSpec change(s), or with an explicit reviewed
`no-spec` rationale. The two views SHALL have exact back-references and matching
projects. The change mapping SHALL distinguish proposed, active,
delivered-awaiting-archive, and archived states, and all invariants SHALL be
verifiable without network access. A delivered-awaiting-archive entry SHALL carry
an ISO delivery date and SHALL fail validation after the documented seven-day
reconciliation window.

#### Scenario: An active change has no issue or rationale

- **WHEN** the offline delivery-index gate finds an active change with neither a Linear identifier nor an explicit rationale
- **THEN** it fails and names the unmapped change

#### Scenario: An active delivery issue has no change or rationale

- **WHEN** the offline gate finds an issue-centric inventory row with neither a mapped OpenSpec change nor an explicit `no-spec` rationale
- **THEN** it fails and names the unmapped issue

#### Scenario: Only one side records an issue/change link

- **WHEN** an issue maps a change that does not map back, or a change owner is absent from the issue inventory
- **THEN** the offline gate fails and names the asymmetric issue/change link

#### Scenario: An archived change is still called active

- **WHEN** a roadmap or delivery-index entry points to an archived change as in-flight
- **THEN** the gate fails and names both the stale reference and archive path

#### Scenario: Delivered work exceeds the archive window

- **WHEN** a delivered-awaiting-archive entry is more than seven days old
- **THEN** the offline gate fails and names the overdue change and reconciliation window

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

The reconciliation report MAY augment local state with status, project, and
issue-membership data from a separately exported read-only Linear snapshot, but
SHALL remain useful offline and SHALL NOT contact or mutate Linear. Issue closure
and scope decisions require the documented maintainer checklist.

#### Scenario: CI has no Linear credentials

- **WHEN** the reconciliation report runs in ordinary CI without external credentials
- **THEN** it validates and reports repository lifecycle truth without attempting a network call

#### Scenario: A reviewed tracker export contains a new active delivery issue

- **WHEN** an optional read-only snapshot contains an active issue in a delivery project that is absent from the checked-in issue inventory
- **THEN** the report fails and names the missing issue without modifying Linear
