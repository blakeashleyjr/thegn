## ADDED Requirements

### Requirement: podman-rootful is not offered on macOS

The `podman-rootful` sandbox backend (`sudo -n podman`) SHALL be runnable only
on Linux. On macOS, podman runs rootless inside a Linux VM and there is no host
root daemon to `sudo` into, so the backend SHALL NOT be a candidate: the default
backend chain SHALL skip it without spawning a probe, and an explicit
`backend = "podman-rootful"` SHALL be reported unsuitable rather than run.

#### Scenario: Default chain on macOS does not probe podman-rootful

- **WHEN** the host is macOS and the resolver walks the default backend chain
- **THEN** `podman-rootful` is rejected by the OS gate before any runtime probe
- **AND** no `sudo -n podman` process is spawned

#### Scenario: Explicit podman-rootful on macOS names the right route

- **WHEN** the host is macOS and `backend = "podman-rootful"` is configured
- **THEN** the backend is reported not runnable "on macOS"
- **AND** the remedy says to use `podman machine set --rootful` with the
  `podman-rootless` backend
