# Plugin API — v0.3 alignment delta

## ADDED Requirements

### Requirement: Plugin support facts have one canonical source

The plugin contract SHALL maintain one machine-readable support table covering
every extension point and host verb with its wire version, support state,
required surface capability or control scope, permitted mode, and owner for
reserved work. Loader negotiation, `plugin check`, schema/docs assertions, and
runtime dispatch MUST agree with it. Decoding an enum variant MUST NOT by itself
mark that surface supported.

#### Scenario: Reserved PanelSection is declared

- **WHEN** a v0.3 plugin requests `PanelSection` before THE-108 lands
- **THEN** decoding succeeds for compatibility but negotiation rejects the
  contribution with a stable reserved/unsupported diagnostic

#### Scenario: Documentation drifts from the host

- **WHEN** help calls a reserved extension point wired
- **THEN** the contract drift test fails naming that extension point

### Requirement: Plugin API v0.3 is additive over v0.2

The API version, committed JSON schema, wire documentation, and host negotiation
SHALL all identify v0.3. New multi-row and theme-slot fields SHALL default so a
v0.2 plugin's single-line view continues to decode, negotiate, serialize, and
render compatibly. Unsupported major/newer requirements SHALL fail with stable
diagnostics.

#### Scenario: v0.2 plugin connects to v0.3 host

- **WHEN** it registers a single-line role-styled view
- **THEN** the host accepts and handles it with the established v0.2 behavior

### Requirement: Exec scope is documented and enforced exactly

Plugin manifests and host calls SHALL treat `read`, `write`, `git`, and `exec`
as independent scopes, with `admin` implying all. A surface capability or plugin
registration MUST NOT grant `exec`; process-launching host capabilities require
the declared `exec` scope in the same central authorization lattice.

#### Scenario: Write-only plugin requests execution

- **WHEN** a plugin granted `write` but not `exec` calls an exec-scoped host
  capability
- **THEN** the host denies and audits the call with the stable permission error
