## ADDED Requirements

### Requirement: The managed pi is acquired through the managed-tool resolver

The managed `pi` binary under `~/.thegn/pi` SHALL be described as a
`managed-tools` spec and acquired through the shared resolver rather than a
bespoke install path. `thegn agent setup` MUST remain idempotent and preserve
its observable behavior: install/refresh the pinned pi, always re-seed the
`thegn-acp` package, register it, and record the pinned version marker.

#### Scenario: agent setup installs via the shared resolver

- **WHEN** `thegn agent setup` runs and the pinned pi is not yet current
- **THEN** the pinned pi is installed through the managed-tool resolver and the
  `thegn-acp` package is (re)seeded and registered

#### Scenario: agent setup is idempotent when already pinned

- **WHEN** `thegn agent setup` runs and the pinned pi is already at the pinned
  version
- **THEN** the binary install is skipped while the `thegn-acp` package is
  still re-seeded and registered
