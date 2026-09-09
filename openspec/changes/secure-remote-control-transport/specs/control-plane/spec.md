# Control plane

## ADDED Requirements

### Requirement: Remote confidentiality has an explicit threat model and decision

The project SHALL persist an architecture decision covering passive observers,
active interception/downgrade, bearer replay, pairing, terminal streams, event
feeds, gRPC, reverse proxies/tunnels, and certificate/fingerprint ownership. It
SHALL select and document native TLS, a strict trusted termination contract, or
both; each supported topology MUST define which component owns confidentiality
and how the server establishes that topology.

#### Scenario: A deployment chooses a supported topology

- **WHEN** an operator exposes the control plane beyond loopback
- **THEN** documentation and config identify the selected encryption boundary
  and its certificate/fingerprint or trusted-terminator ownership

### Requirement: Plaintext non-loopback exposure fails safe

Loopback SHALL remain the safe default. A plaintext non-loopback listener that
is not behind a server-verifiable supported secure boundary MUST be rejected by
default. If an unsafe escape hatch is retained, it SHALL require an explicit,
clearly named opt-in from a trusted config layer and MUST NOT be enabled by
defaults, repository overlays, or an unrelated bind-address change.

#### Scenario: Bind widening alone is refused

- **WHEN** configuration changes the listener from loopback to `0.0.0.0`
  without selecting a supported secure topology or unsafe opt-in
- **THEN** validation/startup refuses the listener with actionable guidance

### Requirement: Every TCP control surface shares confidentiality policy

HTTP requests, WebSocket/SSE event and PTY streams, gRPC, pairing, and advertised
control URLs SHALL derive from the same resolved confidentiality policy. No
surface MAY remain plaintext or advertise an insecure scheme while another
claims the endpoint is secure.

#### Scenario: Secure HTTP implies secure streams

- **WHEN** a secure remote topology is selected
- **THEN** HTTP, WebSocket, and gRPC integration tests all traverse the selected
  encryption boundary and retain scoped token authorization

### Requirement: Pairing and URL handling do not leak bearer material

Pairing and advertised control URLs SHALL encode the actual selected scheme and
topology. Token material MUST NOT be logged or persisted in audit text or
generated deployment artifacts; encryption MUST NOT replace required bearer
scope checks.

#### Scenario: A pairing flow is logged

- **WHEN** pairing creates or displays a secure remote connection
- **THEN** diagnostics may name the endpoint/topology but omit the token and
  persisted URLs contain no reusable bearer credential

### Requirement: Unsafe or incomplete topology is diagnosable

Config validation, startup, and `thegn doctor` SHALL distinguish safe loopback,
supported tunneled/terminated/native-TLS operation, explicit unsafe exposure,
and incomplete/misconfigured secure topology, with actionable recovery. Tests
and architecture/config/CLI/deployment documentation SHALL cover the selected
contract.

#### Scenario: Terminator configuration is incomplete

- **WHEN** config selects trusted TLS termination but cannot establish its
  required peer/header/certificate boundary
- **THEN** startup or doctor fails/surfaces the missing component and does not
  silently classify plaintext traffic as secure
