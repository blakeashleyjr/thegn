# architecture-gates — decision delta for define-gui-frontend-lane

## ADDED Requirements

### Requirement: A future graphical frontend preserves shell architecture

Any future graphical frontend design MUST be additive and MUST keep
`thegn-host` as an independently operable reference frontend. The sanctioned
native candidate SHALL be a separate client of the daemon and the
`thegn_core::capability::CATALOG` control projections, MUST NOT introduce a
second capability registry or GUI-only authority, and MUST NOT add a windowing,
GPU, font, terminal-emulator, or widget substrate to `thegn-core` or the TUI's
render path. Native chrome MUST wait for a stable, serializable view model.

This archived requirement records the constraint for a future implementation;
THE-40 itself SHALL add no crate, dependency ban, owner-table entry, route,
capability, config key, database change, test, or ratchet.

#### Scenario: The decision record is published

- **WHEN** THE-40 is completed
- **THEN** the dated design and archived OpenSpec record identify candidate 2,
  a separate observer-first GPU cell client, as the preferred future lane
- **AND** the existing shell, catalog, control surface, dependencies, and
  architecture gates remain unchanged

#### Scenario: A frontend implementation is proposed later

- **WHEN** a future change introduces a graphical client and toolkit substrate
- **THEN** that change owns the new leaf client crate, dependency-boundary
  decision, security behavior, tests, and applicable ratchet updates
- **AND** `thegn-core` and `thegn-host` remain free of the GUI substrate

#### Scenario: The future client is idle

- **WHEN** the separate frontend has no input, transport event, or pending work
- **THEN** it waits on its own event sources and does not add a polling timeout
  or wake source to the shell's compositor loop

#### Scenario: The client invokes an action

- **WHEN** the future frontend performs an operation
- **THEN** it uses a catalog-projected control capability with the existing
  scope and token policy rather than direct PTY/database access or a GUI-only
  verb
