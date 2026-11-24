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
- **AND** generated Apple network and container names use lowercase portable
  characters while retaining a unique per-run suffix
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

#### Scenario: Process-limit output remains observational without a user control

- **WHEN** V-AC-03 reports a process count under the image's default user
- **THEN** the runner records the count as observed evidence
- **AND** it does not infer that `nproc` enforcement is disabled or approved
  without a suitable user/control comparison

### Requirement: Apple never receives a host or none network keyword

Apple's `container` CLI SHALL NOT be invoked with `--network host` or
`--network none`. `none` is read as a network name rather than removing the
network, and Apple has no host-network mode at all. A launch that resolves
either mode on the `apple` backend MUST be refused with `network-unexpressible`
before any argv is composed. `network = "none"` (and therefore the `sealed`
profile) stays refused while an Apple internal network still reaches services
bound on the Mac, so thegn SHALL NOT report an Apple launch as `Offline` or as
`network = "none"` enforced.

#### Scenario: Apple host is refused before argv

- **WHEN** an `apple` launch resolves `network = "host"`
- **THEN** admission refuses with `network-unexpressible` and no container is
  created
- **AND** no composed argv carries `--network host`

#### Scenario: Apple none names the host-reachability caveat

- **WHEN** an `apple` launch resolves `network = "none"` (including through the
  `sealed` profile)
- **THEN** admission refuses with `network-unexpressible`
- **AND** the reason says the Apple internal network still reaches services
  bound on the Mac, and points at podman or docker for offline work

#### Scenario: Podman and Docker network none is unchanged

- **WHEN** a local podman or docker launch resolves `network = "none"`
- **THEN** its create argv still carries `--network none`
- **AND** the launch is not refused by the Apple-only gate
