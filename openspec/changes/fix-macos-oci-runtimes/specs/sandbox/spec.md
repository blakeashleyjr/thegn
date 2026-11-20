## ADDED Requirements

### Requirement: macOS OCI runtime claims require native evidence

The macOS on-device evidence runner SHALL capture Apple `container` and
available Podman/Docker runtime behavior without treating a command failure as
proof that a sandbox boundary was enforced. It SHALL keep its evidence private
and SHALL only clean resources created by that run. The current implementation
MUST continue to refuse an isolation mode whose enforcement has not been
verified on the selected runtime.

#### Scenario: Internal-network evidence has working positive controls

- **WHEN** the V-AC-02 internal-network check is run
- **THEN** an ordinary container first proves startup, internet reachability,
  and access to a single-use host canary listener
- **AND** the check reports the internal network as isolated only when it
  produces explicit network-denied results for both destinations
- **AND** startup failure, absent listener, missing output marker, or timeout
  is reported as inconclusive

#### Scenario: Evidence cleanup targets only this run

- **WHEN** the runner creates its Apple network, label-test container, listener,
  and evidence files
- **THEN** each runtime resource uses a per-run random name and cleanup is
  limited to those names
- **AND** the listener serves only a random canary body on one chosen interface
  and an ephemeral port
- **AND** output is stored in a private directory, does not overwrite an earlier
  run, and redacts username, home path, hostname, host address, and selected
  image reference

#### Scenario: Runtime-dependent flags remain evidence-gated

- **WHEN** native Mac evidence for an Apple runtime flag is absent, inconclusive,
  or negative
- **THEN** thegn SHALL NOT emit or rely on that flag to claim stronger network,
  process-limit, or label-based lifecycle enforcement
- **AND** any later behavior change SHALL be covered by the reviewed evidence
  and runtime-specific tests
