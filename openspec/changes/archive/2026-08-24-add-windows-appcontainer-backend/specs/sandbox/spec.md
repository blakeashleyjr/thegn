# Sandbox

## ADDED Requirements

### Requirement: The isolation class names the mechanism that actually confines

Every backend SHALL report the boundary it actually has. A backend whose
confinement is the operating system's own access control — no namespaces, no
cgroups, no separate kernel — SHALL report `os-access-control`, which sits below
`shared-kernel`: it constrains what a process may *ask for*, not what the kernel
will *execute*, and every syscall still runs in the host kernel with its full
ABI surface.

A backend that applies no confinement at all SHALL report `host-process`,
whether or not it bounds process lifetime or resources. Lifetime and resource
limits are not a security boundary and MUST NOT be reported as one.

#### Scenario: A token boundary is not reported as a container

- **WHEN** `thegn doctor` describes a backend confined by an OS token (a Windows
  AppContainer)
- **THEN** it reports `os-access-control`, not `shared-kernel`, and its escape
  note says every syscall still runs in the host kernel

#### Scenario: A lifetime-only mechanism is not reported as isolation

- **WHEN** a backend bounds only process lifetime and resources (a Job Object)
- **THEN** it reports `host-process`

### Requirement: Containment is verified from the argv wherever it is visible

Where a backend's containment is expressed in the launch argv, the truth check
SHALL read it and report what actually runs, rather than trusting the request. A
backend MAY be exempted from that check only while its containment is genuinely
invisible to argv inspection — applied inside the spawn syscall — and an
exemption MUST NOT be extended to a backend whose argv does show it.

#### Scenario: A pane that lost its containment is reported as degraded

- **WHEN** a backend whose containment is argv-visible is requested, but the argv
  that runs is a plain host shell
- **THEN** the launch is reported as degraded with the observed label, not as the
  requested one
