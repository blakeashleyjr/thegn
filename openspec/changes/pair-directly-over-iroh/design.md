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
the control API. The client pins the invited endpoint ID during the Iroh
handshake and aborts before sending the pairing code if the server identity is
wrong. The pairing code itself is not bound to an endpoint in the redemption
store; the bearer token authorizes each later protected control operation,
including after reconnect.

The listener is opt-in from `thegn serve`. It is not started for ordinary
pane daemons, and it never forwards requests through the local Unix socket.
This prevents an authenticated remote request from inheriting local-admin
authority.

With the `iroh` feature and dev release channel, `thegn serve --iroh-direct`
starts the listener and prints its Peer ID, concrete IP socket addresses, and
the command to create an invitation. It does not print the invitation or its
code. `--no-pair-url` is supported in direct mode but redundant. An ordinary
pane daemon does not start the listener.

`thegn pair direct new --endpoint-id <peer-id> --address <ip:port> --out
<path>` creates an owner-only invitation file; `--address` may be repeated,
with optional `--scope` and `--ttl-mins`. `thegn pair direct redeem <name>
--invitation-file <path>` redeems it and saves a profile, or reads from stdin
when the flag is absent. `thegn pair direct me <name>` makes a request through
the saved profile. `GET /health` and `GET /pair` are public, `POST /v1/pair`
redeems the code, and later protected routes require a bearer token with their
required scope. The configured approval policy may park a newly redeemed
token pending approval. Pairing credentials and control tokens are not
included in logs.
