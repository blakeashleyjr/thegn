# Remote control transport security

Status: accepted architecture decision for THE-101.

## Decision

Thegn does not terminate TLS natively in this release. `thegn serve` exposes
one plaintext backend carrying HTTP, WebSocket/SSE, and gRPC. Confidential
remote access uses a strict, explicitly declared TLS-termination or encrypted-
tunnel boundary. In either secure topology the backend bind must be loopback;
startup and config validation reject a public backend even if an advertised URL
claims HTTPS.

Scoped bearer authorization remains mandatory on every TCP request. Transport
confidentiality and authorization are independent controls.

## Threat model

The control plane transports terminal output and scrollback, keystrokes,
commands, repository metadata, event feeds, gRPC messages, bearer tokens, and
short-lived pairing codes. A passive observer on a plaintext path can read all
of them. An active intermediary can alter commands and terminal streams,
downgrade a connection, steal and replay a bearer until revocation/expiry, or
redeem a pairing code before its intended recipient.

Pairing codes are single-use and expiring, and only their hashes are persisted,
but possession before redemption is authority. Web pairing keeps the code in a
URL fragment so it is absent from HTTP access logs. Pairing values are printed
only on the explicit one-time CLI/startup surface; token values are not emitted
through tracing, audit records, generated proxy configuration, or `Debug`.
TLS does not make a token safe to log or persist.

Reverse-proxy headers are not a security boundary. Thegn does not trust
`Forwarded` or `X-Forwarded-*` to prove TLS, choose a scheme, or authorize a
caller. The configured topology and advertised endpoint are authoritative, and
the loopback bind makes the same-host terminator/tunnel the only network path to
the plaintext backend.

## Supported topologies

### Direct loopback

```toml
[serve]
bind = "127.0.0.1:5380"
topology = "direct"
```

The client-facing schemes are `http`, `ws`, and `grpc`. This is safe only as a
same-host endpoint and is the default. An SSH client may also forward this port
without changing server configuration, but pairing URLs will describe the
server-local endpoint unless the tunnel topology is declared.

Safe direct mode may advertise only a numeric loopback address or a
`localhost` name. A public `advertise_host` is rejected rather than emitting a
remote plaintext URL for a server that is actually loopback-bound. Host values
are DNS/IP-only; URL delimiters, escapes, embedded ports, and malformed IPv6 or
DNS names fail validation before a pairing code is minted.

### Same-host TLS termination

```toml
[serve]
bind = "127.0.0.1:5380"
topology = "tls-terminated"
advertise_host = "thegn.example.com"
advertise_port = 443
```

The operator-owned reverse proxy listens publicly with TLS and forwards all of
HTTP, WebSocket upgrades/SSE, and gRPC (HTTP/2) to the loopback backend. The
proxy owns certificate issuance, renewal, private-key custody, TLS versions,
and public trust. Pairing advertises `https`; WebSocket and gRPC diagnostics
advertise `wss` and `grpcs`. External termination uses normal platform trust,
so the native fingerprint field remains empty. Private-CA deployments must
install that CA in clients.

A terminator on another machine is not supported by this contract because it
would require a non-loopback plaintext hop. Put an encrypted tunnel between the
machines and keep Thegn loopback-bound instead.

### Encrypted tunnel

```toml
[serve]
bind = "127.0.0.1:5380"
topology = "tunnel"
advertise_host = "127.0.0.1"
advertise_port = 5380
```

The operator owns and authenticates the SSH or equivalent encrypted tunnel.
The advertised endpoint is the address a client sees at its end of the tunnel,
so its application schemes remain `http`, `ws`, and `grpc`; the encrypted
outer tunnel owns confidentiality. Thegn neither provisions nor asserts the
health of that external tunnel.

## Unsafe escape hatch

Direct plaintext on a non-loopback address is rejected by default. Emergency
or isolated-lab use requires the exact global setting
`unsafe_allow_plaintext_non_loopback = true` or CLI flag
`--unsafe-allow-plaintext-non-loopback`. Changing only `bind` cannot imply this
authority. Repository `.thegn.*` overlays have no `[serve]` shape, so checked-in
project configuration cannot enable it.

The unsafe mode exposes bearer and pairing material plus terminal traffic to
capture, replay, and modification. A firewall or private-looking interface
does not change the reported `unsafe-plaintext` state. Use a loopback trusted
terminator/tunnel to restore the supported contract.

## Diagnostics and incident response

`thegn config validate`, startup, and `thegn doctor` distinguish
`safe-loopback`, `tls-terminated`, `tunnel`, `unsafe-plaintext`, and `invalid`.
An invalid topology reports the missing host, contradictory unsafe flag, or
non-loopback backend and refuses startup. If a bearer or pairing code may have
crossed plaintext, revoke its public pairing id with `thegn pair revoke` and
mint a replacement after the secure boundary is restored.
