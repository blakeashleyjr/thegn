# Sandbox

## ADDED Requirements

### Requirement: Every repo-influenced launch has a launch class

thegn SHALL assign exactly one launch class to every spawn of repo-influenced code: `InteractiveOwner` for a
pane the user opens or attaches to, `Untrusted(kind)` for headless and automated launchers, or `HostTrust(kind)`
for a declared host-trust exception. The class SHALL be orthogonal to the launch kind (interactive PTY versus
batch command). `HostTrust` launches MUST be listed in `thegn doctor` and scoped to a command digest. A
repository MUST NOT widen the defaults of a class. The user's own interactive panes SHALL keep the `standard`
default profile; only untrusted classes default to `sealed`, `private-copy`, a private home and an `offline` or
`agent` network, and only once their route is admitted.

#### Scenario: A launcher without a class is not spawnable

- **WHEN** a code path requests a repo-influenced spawn without a launch class
- **THEN** the request is rejected at admission and nothing is spawned

#### Scenario: Interactive default unchanged

- **WHEN** the user opens a worktree shell or interactive agent pane
- **THEN** the class is `InteractiveOwner` and the profile is `standard`

#### Scenario: Host-trust exceptions are visible

- **WHEN** a user hook, an MCP server or an editor is launched on the host
- **THEN** `thegn doctor` lists it as a host-trust route scoped to its command digest

#### Scenario: A repository cannot widen a class default

- **WHEN** a checked-in repository overlay requests networking or a weaker profile for an untrusted class
- **THEN** the class default stands and the request is recorded as ignored

### Requirement: Untrusted launches are admitted only through one handle

An untrusted launch SHALL start only with a `LaunchAdmission` obtained from the single admission function,
which takes the launch purpose, source trust, the resolved user ceiling, the workspace and storage generation
and the candidate command. The handle MUST be produced only after validating mount source, destination and
access mode, the image digest, home view, environment channels, network policy hash, capabilities, runtime
implementation, limits and process scope. The handle's fields SHALL be private, so that a refused request has
no value that a spawn helper accepts. Environment selection SHALL happen before the final intersection of
user, repository and per-run policy, and denies MUST accumulate and win.

#### Scenario: A refused request cannot reach a spawn

- **WHEN** admission refuses a request for an untrusted launch
- **THEN** no spawn helper can be called for it, and the route's `no_host_spawn_after_refusal` test passes

#### Scenario: A named environment cannot widen the ceiling

- **WHEN** a launch selects a named environment whose policy is broader than the user's ceiling
- **THEN** the effective policy is the intersection and the broader grant is not applied

#### Scenario: A repository overlay cannot widen the class default

- **WHEN** a repository overlay sets a broader network preset than the class default
- **THEN** admission applies the class default

#### Scenario: Every route is a handle or a host-trust exception

- **WHEN** `LAUNCH_ROUTES` is evaluated for queue agents, autopilot, merge gates, regeneration, tests and discovery, repository lifecycle scripts, language services, headless tools, provider exec and untrusted plugins
- **THEN** each row reads `handle`, or is a declared host-trust exception shown in `thegn doctor`

### Requirement: Adapters report enforced controls and an effective-policy identity

thegn SHALL compute the controls an adapter enforces; it MUST NOT accept a declared claim. Enforced controls
SHALL derive from the enforcement manifest, the host report and, for adapters that are not built in, a live
attestation. A launch SHALL carry an immutable effective-policy identity derived from the resolved policy, the
egress policy digest, the workspace generation, the backend definition and binary digests and the manifest
revision, and reuse of existing compute SHALL require an equal identity. A changed static control SHALL stop
and recreate compute without deleting workspace data. Audit records SHALL compare requested, resolved and
observed properties and MUST NOT contain credential values.

#### Scenario: A declared control without evidence is not enforced

- **WHEN** a config-defined backend declares a network deny that no manifest row, host report or attestation supports
- **THEN** the enforced controls do not include it and a policy requiring it is refused

#### Scenario: Reuse requires an equal identity

- **WHEN** a retained container's recorded policy identity differs from the one computed for the new launch
- **THEN** the container is not reused and is recreated without touching workspace data

#### Scenario: Canaries observe the enforced controls

- **WHEN** the canary corpus runs a synthetic home, sibling, Git, cache and system path probe on an admitted backend
- **THEN** every probe is denied, and a direct IP, an alternate DNS, an IPv6 destination and a proxy bypass are blocked under an offline or agent policy

### Requirement: Unqualified backends refuse secure policies

A backend SHALL satisfy a secure policy (any untrusted class or a `sealed` profile) only when its enforced
controls include a private filesystem scope, a hidden or denied home, own-namespace, virtual machine or
verified OS-level network isolation, and durable private storage for private-copy, and its verification on
that operating system is verified or attested. systemd, Landlock-only, native Windows Job Object and host
execution MUST be refused for secure policies. Seatbelt, MXC, WSL and virtual-machine engines SHALL be refused
until their evidence exists. Every refused combination SHALL appear in the refusal table with a test.

#### Scenario: systemd is refused for a sealed untrusted launch

- **WHEN** an untrusted launch with a secure policy resolves to a systemd backend
- **THEN** admission is refused with a reason naming the missing filesystem boundary

#### Scenario: Unverified Seatbelt is refused

- **WHEN** a secure policy resolves to Seatbelt before its verification evidence exists
- **THEN** admission is refused, and doctor reports the evidence as missing

#### Scenario: Native Windows isolation is refused

- **WHEN** a secure policy resolves to a Windows Job Object
- **THEN** admission is refused because it is a host-process class

#### Scenario: A qualified backend is admitted

- **WHEN** a secure policy resolves to bwrap or an OCI backend whose enforced controls cover the requirement
- **THEN** admission mints a handle carrying the effective-policy identity

### Requirement: A secure refusal never spawns on the host

A secure request MUST NOT become a host spawn after a refusal, a failed probe, a failed image preparation, a
runtime or gateway loss or an exec failure. Runtime or gateway loss SHALL surface as `runtime-lost` with a
recoverable identity and MUST NOT delete data, reset a baseline or fall back to the host. Where an enforcement
or audit mechanism that is part of the requested policy is unavailable, thegn SHALL reject the launch and make
the unavailability visible.

#### Scenario: Image preparation fails

- **WHEN** image preparation fails for an untrusted launch under a secure policy
- **THEN** the launch is refused and no process is started on the host

#### Scenario: Probe failure on every route

- **WHEN** the backend probe fails for each launch route of an untrusted class
- **THEN** each route refuses and its `no_host_spawn_after_refusal_<route>` test observes zero host spawns

#### Scenario: Runtime loss does not fall back

- **WHEN** the runtime of an admitted launch is lost
- **THEN** the workspace is reported as `runtime-lost`, no host process is started and no data is deleted or reset

#### Scenario: Required audit is unavailable

- **WHEN** the audit sink required by the policy is unavailable at admission
- **THEN** the launch is rejected and the missing mechanism is reported
