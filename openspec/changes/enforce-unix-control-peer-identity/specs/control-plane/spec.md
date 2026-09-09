# Control plane

## ADDED Requirements

### Requirement: Unix peer credentials reach authorization context

On supported Unix platforms, the listener SHALL retrieve each accepted peer's
effective UID with the native credential API and carry an immutable verified,
mismatched, or unavailable peer result through HTTP, WebSocket, and gRPC
authorization context. A socket path or listener-level locality flag MUST NOT
stand in for authenticated peer identity.

#### Scenario: Credential retrieval succeeds

- **WHEN** a Unix client connects to the local control socket
- **THEN** request authorization receives the peer effective UID captured from
  the accepted stream

### Requirement: Implicit local Admin requires same effective UID

When `local_admin = true`, implicit Admin SHALL be granted only when a verified
peer effective UID equals the daemon effective UID. Mismatched or unavailable
credentials SHALL require an otherwise valid scoped token or be rejected; they
MUST NOT be silently upgraded. When `local_admin = false`, all peers SHALL use
ordinary token policy regardless of UID.

#### Scenario: Same UID succeeds implicitly

- **WHEN** local Admin is enabled and the verified peer/daemon UIDs match
- **THEN** the local request receives implicit Admin

#### Scenario: Unknown UID fails closed

- **WHEN** credential retrieval is unavailable and no token is supplied
- **THEN** authorization rejects the request rather than granting Admin

#### Scenario: Local Admin is disabled

- **WHEN** a same-UID peer connects with `local_admin = false`
- **THEN** it must present the ordinary scoped token

### Requirement: Local Admin requires secure socket ownership and modes

With implicit local Admin enabled, startup SHALL verify owner-controlled run
directory/socket ownership and restrictive modes. A hardening failure SHALL be
surfaced by startup/doctor and MUST disable/refuse implicit Admin rather than
continue as a silent best-effort downgrade.

#### Scenario: Socket chmod fails

- **WHEN** the daemon cannot establish the required private socket mode
- **THEN** it refuses or disables implicit local Admin and reports the exact
  remediation

### Requirement: Token-required portability mode is explicit

Platforms or filesystems without reliable Unix peer credentials or ownership/
mode enforcement SHALL have a documented token-required mode. That mode SHALL
retain the local socket transport without claiming same-UID authentication.

#### Scenario: Peer credentials are unsupported

- **WHEN** the platform adapter cannot authenticate a peer UID
- **THEN** scoped tokens remain usable and docs/doctor identify that implicit
  Admin is unavailable

### Requirement: Peer authorization is covered and documented

Tests SHALL cover same-UID success, mismatched UID with/without token,
unavailable UID, `local_admin = false`, and permission-hardening failure.
Comments and architecture/configuration docs MUST describe enforced behavior,
not a permissions-only assumption.

#### Scenario: A comment promises same UID

- **WHEN** local-socket authorization is documented as same-UID
- **THEN** the corresponding integration test proves native peer verification
