# Design — remote transport confidentiality decision

## Threat model to close

The decision record must cover passive capture, active interception/downgrade,
bearer replay, pairing URL handling, PTY input/output, event feeds, gRPC,
reverse proxies, SSH/VPN/Iroh tunnels, certificate/fingerprint ownership, and
misconfigured forwarded headers. The control plane carries code, terminal
contents, and credentials; confidentiality is mandatory independently of
scope authorization.

## Supported topology contract

Implementation begins by selecting and documenting one or both of:

1. Native TLS owned by Thegn, including certificate/key/fingerprint lifecycle.
2. Plaintext listener restricted to loopback or a verified trusted local
   termination/tunnel boundary, with the public hop encrypted by an explicitly
   supported topology.

If the server cannot verify a termination boundary, a non-loopback plaintext
bind is unsafe. It is rejected unless a clearly named unsafe opt-in is present
in a trusted configuration layer. That opt-in is never a default and cannot be
widened by repository config.

## Common policy projection

One resolved security mode feeds the TCP listener, HTTP middleware,
WebSocket/SSE upgrade, gRPC server, pairing URL construction, advertised URLs,
config validation, startup diagnostics, and doctor. No transport is allowed to
interpret `secure` differently.

Pairing/control URLs represent the actual secure scheme/topology and never
embed bearer material in logged/persisted URL text. Reverse-proxy configuration
must state which headers are trusted and from which peer boundary; arbitrary
forwarded headers do not prove TLS.

## Verification

Integration tests cover the loopback default, non-loopback refusal, explicit
unsafe escape hatch if retained, the selected secure native/proxy topology,
pairing/advertised URLs, token authorization under encryption, and policy parity
across HTTP, WebSocket, and gRPC.
