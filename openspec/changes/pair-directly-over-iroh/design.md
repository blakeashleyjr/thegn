# Design

The direct control listener uses its own Iroh ALPN and key, separate from the
sandbox `ExecReq` listener. It serves the existing Axum control router with
`local_admin = false`, so `/v1/pair` consumes a one-time code through the
existing atomic redemption path and subsequent requests verify the minted
scoped token on every call. Revocation therefore remains effective without a
second token store or authorization table.

The client transport is a `ControlAddr` variant in `ControlClient`. It accepts
only an endpoint identity plus one or more explicit IP socket addresses. Its
Iroh endpoint disables relay and address lookup, and its ALPN is specific to
the control API. Iroh's endpoint identity authenticates the selected server;
the bearer token continues to authorize each control operation.

The listener is opt-in from `thegn serve`. It is not started for ordinary
pane daemons, and it never forwards requests through the local Unix socket.
This prevents an authenticated remote request from inheriting local-admin
authority.

Pairing credentials and control tokens are not included in logs. The listener
prints a direct invitation only when explicitly requested; invite parsing
rejects relay/custom addresses, missing addresses, malformed IDs, and
unspecified endpoints.
