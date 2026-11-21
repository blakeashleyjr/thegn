## MODIFIED Requirements

### Requirement: Direct Iroh pairing uses the existing scoped control authority

The opt-in direct Iroh control listener SHALL use a dedicated control ALPN,
explicit direct UDP IP socket addresses, and an endpoint ID authenticated by
the Iroh handshake. It SHALL disable relay and address lookup and SHALL serve
the existing control router with implicit local administration disabled,
and SHALL pass the Iroh-authenticated remote peer endpoint ID as request
metadata to that router. This peer identity metadata SHALL remain distinct
from authorization, which SHALL continue to use the scoped bearer token and
existing approval policy. Pairing SHALL redeem the existing expiring
single-use code through `POST /v1/pair`; `GET /health` and `GET /pair` remain
public. A code MAY be redeemed and a scoped bearer token issued before
approval, but a pending or otherwise unapproved token SHALL NOT authorize
protected routes until the peer is accepted under the existing policy. Every
request to a protected route SHALL present a bearer token with the required
scope, including requests after reconnect, and SHALL verify that token against
the existing revocation store. The direct control transport SHALL remain
separate from sandbox `ExecReq` transport and existing SSH/dumbpipe host
behavior.

With the `iroh` feature and dev release channel, `thegn serve --iroh-direct`
SHALL start the direct listener and print its Peer ID, concrete IP socket
addresses, and the command for creating an invitation. It SHALL NOT print the
invitation or its code. `--no-pair-url` is accepted with direct mode but is
redundant because direct mode does not print a pairing URL. An ordinary pane
daemon SHALL NOT start this listener.

`thegn pair direct new --endpoint-id <peer-id> --address <ip:port> --out
<path>` SHALL create an owner-only invitation file; `--address` MAY be
repeated, and `--scope` and `--ttl-mins` are optional. `thegn pair direct
redeem <name> --invitation-file <path>` SHALL redeem that file and save a
reconnectable profile; when `--invitation-file` is absent, it SHALL read the
invitation from stdin. `thegn pair direct me <name>` SHALL use the saved
profile. If the server endpoint ID reached by the client does not match the
invited ID, the client SHALL abort the Iroh handshake before transmitting the
pairing code. The redemption store does not bind a code to an endpoint ID.

#### Scenario: A direct invitation is redeemed and reused

- **WHEN** a peer redeems a valid invitation, is accepted under any required
  approval policy, and then reconnects to the invited endpoint ID using its
  explicit direct addresses
- **THEN** the peer receives only the code's scopes and can make an authorized
  control request over the separate Iroh control ALPN

#### Scenario: A pending pairing cannot authorize protected routes

- **WHEN** a peer redeems a valid code while its required approval is pending
- **THEN** the scoped token MAY be issued, but it cannot authorize protected
  routes until the peer is accepted under the existing policy

#### Scenario: Direct control has no implicit local administrator

- **WHEN** a QUIC peer connects to the direct control listener without a
  bearer token
- **THEN** protected routes reject the request even if the listener process
  also owns a local Unix socket that grants same-UID administrator access

#### Scenario: Direct pairing is revoked or cannot be redeemed

- **WHEN** a code is expired, already redeemed, invalid, or a redeemed token
  is revoked
- **THEN** the normal pairing/authentication path refuses it and does not grant
  broader local administrator authority

#### Scenario: The invited server identity does not match

- **WHEN** the client reaches an endpoint whose Iroh identity differs from the
  endpoint ID in the invitation
- **THEN** the client aborts the Iroh handshake before transmitting the code

#### Scenario: No direct IP path exists

- **WHEN** the invited peer cannot reach an explicit IP socket address
- **THEN** connection fails with a direct-connect error; the client does not
  query an address lookup service or fall back to a relay, and this mode has
  no Tailscale dependency
