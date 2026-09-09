# Tasks — remote control confidentiality (THE-101)

## 1. Decision and threat model

- [ ] 1.1 Persist the full network/pairing/stream/replay/proxy threat model.
- [ ] 1.2 Select native TLS, strict termination/tunneling, or both; define
      certificate/fingerprint, URL, and trusted-proxy ownership.

## 2. Policy and enforcement

- [ ] 2.1 Add one resolved confidentiality policy shared by HTTP, WebSocket/
      SSE, gRPC, pairing, and advertised URLs.
- [ ] 2.2 Reject plaintext non-loopback by default; if retained, require a
      trusted-layer explicit unsafe opt-in that bind widening cannot imply.
- [ ] 2.3 Preserve scoped bearer checks and redact tokens from URLs, argv,
      logs, audit rows, and generated artifacts.

## 3. Diagnostics and verification

- [ ] 3.1 Add config/startup/doctor states and actionable recovery for safe,
      secure, unsafe-opted-in, and incomplete topologies.
- [ ] 3.2 Add integration tests for loopback default, refusal/unsafe escape,
      selected secure topology, URLs/pairing, auth, and protocol parity.
- [ ] 3.3 Update architecture, configuration, CLI, and deployment docs.

## 4. Reconciliation

- [x] 4.1 Create the active OpenSpec delta linked to THE-101.
- [ ] 4.2 Validate the completed change strictly before archive.
