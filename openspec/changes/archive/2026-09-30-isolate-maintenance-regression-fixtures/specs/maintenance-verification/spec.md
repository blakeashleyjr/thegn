## ADDED Requirements

### Requirement: Watchdog regression work belongs to its fixture

The watchdog regression Harness SHALL retain an existing owned worktree and
private state, and SHALL select native local behavior without admitting a
provider, container backend, daemon attachment or real user shell startup file.
The fixture SHALL exercise actual single-swap behavior before owned cleanup.

#### Scenario: A degraded blank local pane exceeds its deadline

- **WHEN** the owned local regression triggers the real watchdog swap
- **THEN** it SHALL use the fixture worktree and create one local replacement pane
- **AND** a second tick SHALL not create another replacement
- **AND** fixture resources SHALL be released before temporary state is removed

### Requirement: Git regression setup is explicit and owned

Git divergence and merge-state fixtures SHALL establish required branch and
identity values explicitly, without assuming developer global configuration.
A deliberately conflicting merge SHALL prove its expected setup status before
asserting derived merge state. Temporary directories SHALL remain owned through
assertion failure cleanup.

#### Scenario: An empty global Git configuration supplies no branch or identity

- **WHEN** the divergence and conflicting-merge regressions execute
- **THEN** their explicit private setup SHALL produce the intended main upstream and live conflict
- **AND** normal divergence and abort assertions SHALL still execute
