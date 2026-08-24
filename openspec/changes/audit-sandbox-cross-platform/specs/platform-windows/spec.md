# Platform: Windows

## ADDED Requirements

### Requirement: Job-object scoping is never reported as containment

The native Windows `jobobject` backend SHALL be classified as a host process
with process-tree scoping: its kill-on-close Job Object provides lifetime and
(when shipped) resource scoping but no filesystem or network isolation, so its
honest isolation class MUST be the host-process class, and it MUST NOT satisfy
any isolation floor at `shared-kernel` or above. The reserved `appcontainer`
backend, being an OS-enforced security boundary, retains a container-class
reservation to be claimed only when it ships and its enforcement is verified.
The enforcement matrix's Windows column SHALL state that OCI backends are
declined by policy (the real-path bind cannot be honored from a Linux VM) and
that job-object resource limits are deferred, for as long as each remains
true.

#### Scenario: A container floor on native Windows is a miss

- **WHEN** `isolation_floor = "shared-kernel"` is set on native Windows and
  the resolved backend is `jobobject`
- **THEN** the floor is treated as missed and `on_floor_miss` governs the
  outcome (a warning-and-degraded launch by default; a refusal under `fail`)

#### Scenario: The Windows matrix column is honest about scoping

- **WHEN** `thegn doctor` renders the enforcement matrix on native Windows
  with the `jobobject` backend ready
- **THEN** the row shows process-tree scoping present, filesystem and network
  isolation absent, and the host-process isolation class — never a container
  class
