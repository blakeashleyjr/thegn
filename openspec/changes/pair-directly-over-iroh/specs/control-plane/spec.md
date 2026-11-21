## ADDED Requirements

### Requirement: Direct Iroh control pairing

The control service SHALL offer an opt-in direct Iroh transport for its
existing scoped control API. The server SHALL accept direct control connections
only on its separate control ALPN. A client SHALL authenticate the server's
invited Iroh endpoint identity, and the listener SHALL expose the authenticated
remote peer identity to the control API. Direct pairing SHALL redeem the
existing expiring, single-use pairing code and authorize each subsequent
control operation with the resulting scoped bearer token. The direct listener
SHALL require that token and SHALL NOT grant implicit local administrator
authority. This experimental CLI surface SHALL require the dev release
channel. Pairing and bearer credentials SHALL NOT be logged.

#### Scenario: Server explicitly enables direct pairing

- **WHEN** the operator starts `thegn serve` with direct Iroh enabled
- **THEN** the control service exposes its separate direct Iroh listener and
  its direct invitation contains the server peer ID and concrete IP socket
  addresses
- **AND** an ordinary pane daemon does not start this listener

#### Scenario: Peer redeems an invitation

- **WHEN** a peer connects using the invited peer ID and explicit IP socket
  addresses and redeems a valid pairing code
- **THEN** the server identity matches the invited endpoint and the listener
  preserves the authenticated remote peer identity in connection metadata
- **AND** the existing pairing flow returns a token with the granted
  scopes
- **AND** an expired, previously redeemed, or invalid code grants no access

#### Scenario: Scoped authorization and revocation apply to direct requests

- **WHEN** a paired peer makes a direct control request
- **THEN** the control API checks its bearer token and required scope for that
  request without treating Iroh peer identity as authorization
- **AND** requests using a revoked or expired pairing are refused, including
  after reconnecting with a saved pairing profile

#### Scenario: Direct mode has no relay or implicit network path

- **WHEN** an operator or client configures direct Iroh pairing
- **THEN** it uses an explicit server peer ID and one or more concrete IP
  socket addresses, with relay use and address lookup disabled
- **AND** the peers need a working direct IP path, including any required UDP
  traffic
- **AND** this mode does not use Tailscale, a relay server, the sandbox
  `ExecReq` listener, or local Unix-socket administration
- **AND** existing HTTP pairing and `host add dumbpipe:` remain available
