# Plugin Runtime — contract alignment delta

## ADDED Requirements

### Requirement: Runtime negotiation exposes only wired support

The general plugin host SHALL advertise and accept only extension points whose
canonical support state is wired in that runtime. Separately implemented
adapter surfaces SHALL name their owning subsystem, and reserved points SHALL
decode but be rejected before activation. `plugin list` and `plugin check`
SHALL report the same classification without starting resident plugins.

#### Scenario: Manifest mixes wired and reserved contributions

- **WHEN** a manifest requests a wired statusbar segment and reserved panel
  section
- **THEN** negotiation reports each contribution's real state and does not
  silently activate the reserved section

### Requirement: Contract inspection is side-effect free

Version/support/scope inspection and `plugin check` SHALL validate manifests,
commands, and contribution negotiation without starting a resident plugin or
granting a scope not present in trusted configuration.

#### Scenario: Operator checks an exec-capable plugin

- **WHEN** `thegn plugin check` inspects a manifest requesting `exec`
- **THEN** it reports the requested/granted result but does not execute the
  plugin or any host capability
