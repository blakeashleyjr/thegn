## ADDED Requirements

### Requirement: Provider fallback requires authoritative session absence

Native provider recovery SHALL open a fresh session only after a successful,
valid authoritative session roster proves the persisted identifier is absent.
Named sessions MUST remain on their native identity-bearing transport.

#### Scenario: Provider roster is inaccessible

- **WHEN** the roster response is unauthorized, missing, unavailable or malformed
- **THEN** recovery does not interpret it as an empty roster or open a duplicate shell

#### Scenario: A named provider session meets a connected call-home transport

- **WHEN** a persisted native session is restored while an iroh transport is connected
- **THEN** attachment and absence queries use the native provider that owns the identifier
