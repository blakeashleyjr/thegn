# Platform: macOS

## MODIFIED Requirements

### Requirement: Host-toolchain mounts require a matching guest ABI

Every injection of **host** toolchain paths into a container — the toolchain
substrate (`/usr`, `/bin`, `/lib`, `/nix/store`, `$HOME` dotfiles), the Tier-B
Nix daemon socket, and the devenv `/nix` bind — SHALL be gated on the backend's
guest sharing the host's OS and ABI. An OCI guest is always Linux, so on a
non-Linux host all of them MUST be withheld; every other backend executes on the
host's own kernel, so host and guest are the same system and they MUST be kept
regardless of OS. When an explicitly requested injection is withheld, thegn MUST
say so rather than ship a quietly different sandbox.

#### Scenario: A Linux guest on a macOS host gets no host toolchain

- **WHEN** a spec is built for an OCI backend on macOS
- **THEN** no host `/usr`, `/bin`, `/lib` or `/nix/store` bind is emitted —
  mounting them shadows the guest's own binaries, producing "failed to find
  target executable" and "Exec format error" at container start

#### Scenario: The Nix daemon socket and devenv bind are withheld too

- **WHEN** a spec is built for an OCI backend on macOS with `nix_daemon` on, or
  with a devenv path in the Nix store
- **THEN** neither `/nix/var/nix/daemon-socket` nor `/nix` is bound — a host
  nix-daemon serves host-native store paths a Linux guest cannot execute, and
  `/nix` is outside the VM's shared set, so the bind fails the container create
  outright and the backend cannot start at all

#### Scenario: An explicitly requested injection says it was dropped

- **WHEN** `[sandbox] nix_daemon` is enabled and the guest ABI does not match
- **THEN** thegn warns that it was left off and why, rather than silently
  omitting it

#### Scenario: A non-OCI backend on macOS keeps them

- **WHEN** a spec is built for backend `none` on macOS
- **THEN** host toolchain injection is unaffected, because the process runs on
  this host's own kernel

#### Scenario: A Linux host is unaffected

- **WHEN** the same spec is built on Linux
- **THEN** the toolchain mounts are injected exactly as before, because host and
  guest are the same system

## ADDED Requirements

### Requirement: A local OCI backend on macOS is reported as guest-kernel isolation

On macOS an OCI runtime reaches its Linux container through a virtual machine, so
relative to the host the boundary is a guest kernel. thegn SHALL report that
class for a local OCI backend on macOS rather than the shared-kernel class that
describes the same runtime on Linux. An explicitly configured stronger OCI
runtime (`runsc`, `krun`) MUST still win, and Linux reporting MUST be unchanged.

#### Scenario: Doctor reports guest-kernel for podman on macOS

- **WHEN** `thegn doctor` runs on macOS with a local podman or docker backend
- **THEN** the backend's isolation is reported as guest-kernel

#### Scenario: Linux reporting is unchanged

- **WHEN** the same backend is classified on Linux with no stronger OCI runtime
  configured
- **THEN** its isolation is reported as shared-kernel
