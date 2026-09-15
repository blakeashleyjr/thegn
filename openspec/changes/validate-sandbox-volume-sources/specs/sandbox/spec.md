# Sandbox

## ADDED Requirements

### Requirement: Named-volume sources are admitted before execution

Thegn SHALL validate every nonempty `SandboxSpec.volumes` source as a named
volume before constructing an OCI entry argv, creating a container, attaching
a VPN, or executing a host-side launch fallback. The source SHALL be at least
two bytes, begin with an ASCII alphanumeric byte, and contain only ASCII
letters, digits, `_`, `-`, or `.`. Invalid input SHALL produce a terminal,
fallible refusal with bounded redacted metadata.

#### Scenario: Path-like source is refused

- **WHEN** a composed sandbox has `/tmp/state`, `C:\\state`, `~/state`, or
  option syntax as a named-volume source
- **THEN** launch returns a typed refusal before argv, OCI ensure, VPN, or host
  fallback effects, and the message does not echo the source

#### Scenario: Valid name remains usable

- **WHEN** a named-volume source is `cache_01`, `ok-volume`, or `v1.cache`
- **THEN** existing sandbox argv and ensure behavior are preserved

#### Scenario: Final overlays are checked

- **WHEN** Ready-host and remote finalization add or rewrite the volume list
- **THEN** admission checks the final spec before VPN attachment or ensure

#### Scenario: Disabled or absent sandbox remains unchanged

- **WHEN** sandbox configuration is disabled or has no named volumes
- **THEN** the existing host/None path and ordinary launch behavior continue
