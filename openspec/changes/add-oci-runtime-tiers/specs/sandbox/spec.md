# Sandbox

## ADDED Requirements

### Requirement: Selectable OCI runtime raises the isolation tier honestly

The sandbox SHALL accept a `[sandbox] oci_runtime` value that runs a worktree's
OCI container under a named OCI runtime. When set for an OCI backend, thegn MUST
pass `--runtime <value>` at container create, and MUST report the honest
isolation class the runtime provides: `runsc` (gVisor) as `userspace-kernel` and
`krun` (libkrun) as `guest-kernel`; an empty value or `runc`/`crun` stays
`shared-kernel` (today's behavior). Non-OCI backends (bwrap/systemd/none) MUST
ignore the value. The worktree bind MUST remain path-preserving and thegn MUST
keep enforcing egress itself, exactly as for the default runtime.

#### Scenario: gVisor is reported as a userspace kernel

- **WHEN** `[sandbox] oci_runtime = "runsc"` and the backend is an OCI runtime
- **THEN** the container is created with `--runtime runsc` and its resolved
  isolation class is `userspace-kernel`

#### Scenario: libkrun is reported as a guest kernel

- **WHEN** `[sandbox] oci_runtime = "krun"` and the backend is an OCI runtime
- **THEN** the container is created with `--runtime krun` and its resolved
  isolation class is `guest-kernel`

#### Scenario: A non-OCI backend ignores the runtime

- **WHEN** `[sandbox] oci_runtime = "krun"` but the resolved backend is bwrap
- **THEN** no `--runtime` flag is emitted and the isolation class is unchanged
  (shared kernel)

#### Scenario: The worktree bind and egress control are preserved

- **WHEN** a worktree runs under `oci_runtime = "runsc"` or `"krun"`
- **THEN** the worktree is still bind-mounted at its real host path (host git
  reads stay coherent) and thegn's own DNS/egress enforcement still applies

### Requirement: An unavailable OCI runtime degrades to the default, never fails the pane

When `[sandbox] oci_runtime` names a runtime whose host requirements are not met
— its runtime binary is absent from `PATH`, or (for `krun`) `/dev/kvm` is not
present — the sandbox SHALL fall back to the daemon's default runtime and surface
a human-visible warning, rather than failing to create the container. `thegn
doctor` SHALL report the configured runtime and whether it is available on the
current host.

#### Scenario: Missing runtime binary falls back with a warning

- **WHEN** `oci_runtime = "runsc"` but no `runsc` binary is on `PATH`
- **THEN** the container is created under the default runtime and a warning
  notes the fallback

#### Scenario: libkrun without KVM falls back

- **WHEN** `oci_runtime = "krun"` but `/dev/kvm` is not available
- **THEN** the container is created under the default runtime and a warning
  notes that `/dev/kvm` is missing

#### Scenario: doctor reports runtime availability

- **WHEN** a user runs `thegn doctor` with `oci_runtime` set
- **THEN** the sandbox boundary report shows the configured runtime and whether
  it is available (or the reason it would fall back)
