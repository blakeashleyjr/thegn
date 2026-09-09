# Design — remote transport confidentiality decision

## Threat model to close

The decision record must cover passive capture, active interception/downgrade,
bearer replay, pairing URL handling, PTY input/output, event feeds, gRPC,
reverse proxies, SSH/VPN/Iroh tunnels, certificate/fingerprint ownership, and
misconfigured forwarded headers. The control plane carries code, terminal
contents, and credentials; confidentiality is mandatory independently of
scope authorization.

## Decision and supported topology contract

This release selects strict external TLS termination/tunneling rather than
native TLS. Thegn owns no certificate or private key. A same-host TLS terminator
owns issuance, renewal, key custody, public trust, and the encrypted public hop;
an operator-managed SSH/equivalent tunnel owns its encrypted outer hop. Both
topologies require Thegn's native plaintext backend to bind loopback, which is
the server-verifiable boundary preventing arbitrary network peers from reaching
or impersonating the trusted component. `Forwarded` and `X-Forwarded-*` headers
are ignored as security evidence.

`[serve] topology = "direct" | "tls-terminated" | "tunnel"` declares the
boundary. Secure modes require `advertise_host`; `advertise_port = 0` means 443
for TLS termination and the backend port for direct/tunnel. Direct non-loopback
plaintext is rejected unless the global config key or CLI flag named
`unsafe_allow_plaintext_non_loopback` is explicit. Repository overlay schemas
contain no `[serve]` field and cannot grant this authority. Bind widening alone
never enables it.

Safe direct mode may advertise only a loopback IP or `localhost` name. All
advertised hosts are strict DNS/IP values with no URL delimiters, escapes, or
embedded ports, and pairing URL parsing repeats that validation at the trust
boundary. A loopback server therefore cannot claim an unrelated public direct
endpoint, and a host value cannot inject pairing parameters.

## Common policy projection

One resolved `ServeTransportPolicy` feeds the TCP listener, shared HTTP plus
WebSocket/SSE plus gRPC server, pairing URL construction, advertised URLs,
config validation, startup diagnostics, and doctor. No surface interprets
`secure` independently. TLS termination projects `https`/`wss`/`grpcs`; direct
and tunnel modes accurately project `http`/`ws`/`grpc` because tunnel
encryption is outside the application protocol.

Pairing/control URLs represent the actual scheme/topology. Pairing codes remain
in the explicit one-time output and web fragment, are stored only as hashes,
and are redacted from `Debug`; logs/audit rows/generated artifacts never carry
them. External TLS termination uses normal client trust, so native certificate
fingerprints remain unset. Encryption never substitutes for bearer scope checks.

## Verification

Tests cover the loopback default, non-loopback refusal, explicit unsafe escape,
strict loopback TLS/tunnel boundaries, incomplete/contradictory topology,
pairing schemes/redaction, scoped token preservation, diagnostics, and the one
HTTP/WebSocket/gRPC scheme projection.
