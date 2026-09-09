# Define and enforce remote control-plane confidentiality

Linear: THE-101
Parent: THE-41

## Problem

`thegn serve` transports terminal output/input, commands, event streams,
pairing material, and bearer credentials over plaintext HTTP/WebSocket/gRPC.
Loopback is the default and tunnels/terminators are recommended, but an
explicit non-loopback bind can currently widen exposure without a
machine-enforced confidentiality contract.

## Proposed change

- Record a threat model and an explicit architecture decision for native TLS,
  strict trusted TLS termination/tunneling, or a supported combination.
- Treat loopback and verified secure topologies as the normal contract.
  Plaintext non-loopback, if retained at all, requires an unmistakable unsafe
  opt-in that cannot be introduced by accidental defaults or untrusted overlays.
- Apply one confidentiality policy to HTTP, WebSocket/PTY, gRPC, pairing, and
  advertised control URLs.
- Validate config and surface actionable startup/doctor diagnostics for unsafe,
  incomplete, or unverifiable topology.
- Preserve scoped bearer authorization as a separate mandatory layer and keep
  tokens out of URLs, logs, and persistent audit output.

## Baseline retained

TCP always requires scoped tokens; loopback `127.0.0.1:5380` is the default;
CORS defaults closed and rejects wildcard; pairing tokens expire, are
single-use, and are stored hashed; TCP never grants local implicit admin.

## Non-goals

- Operating a public CA or configuring arbitrary VPNs/firewalls.
- Treating encryption as authorization or removing scoped bearer tokens.
- Solving local Unix peer identity (THE-100).
