# Platform: Windows

## MODIFIED Requirements

### Requirement: Container backends are declined on native Windows with the reason

Backend selection MAY pick an OCI runtime (podman/docker/smol) on native Windows
when — and only when — both halves of the old blocker are addressed:

1. **Mount destinations** are mapped deterministically. `Mount` carries `host`
   and `dest` separately, and every destination MUST be produced by
   `sandbox::container_path`, which maps `C:\…` into the `/mnt/<drive>/…` tree a
   WSL-backed machine already exposes. This applies to the worktree, the
   git-common dir, the host-toolchain and cache mounts, the OCI `--workdir`, the
   preflight probe, and the bind-source comparison used to decide whether a
   running container still matches its spec. A destination left as a Windows path
   emits `-v C:\…:C:\…`, which the runtime rejects, and the container never
   starts.
2. **Git metadata** resolves under that mapping. Every thegn tab is a *linked*
   worktree whose `.git` is a pointer file carrying an absolute host path, so the
   sandbox MUST be given rewritten `.git` and `gitdir` pointers
   (`sandbox_gitshim`). Without them git inside the container reports
   `not a git repository: (null)`.

A sandbox in which in-worktree `git` cannot resolve its own gitdir SHALL NOT be
selected. Should either half regress, selection MUST decline the OCI runtimes on
native Windows again and name the actual reason.

#### Scenario: Docker Desktop installed

- **WHEN** backend `auto` resolves on native Windows with `docker` on PATH and
  answering
- **THEN** docker is selected, and `git status` inside the pane reports the same
  HEAD the host does

#### Scenario: Preflight and status probes use the mapped path

- **WHEN** thegn composes the OCI preflight `exec` or verifies a running
  container's binds
- **THEN** both are expressed in the runtime's namespace via `container_path`, so
  the probe's `--workdir` exists and a correct container is never force-recreated

#### Scenario: A worktree whose metadata cannot be mapped

- **WHEN** a worktree's git metadata cannot be made to resolve under the mapped
  destination
- **THEN** the OCI runtime is not selected for it, and the decline names the
  git-metadata reason rather than a mount-path one

## ADDED Requirements

### Requirement: A backend that applies no containment MUST probe Absent

A backend SHALL report itself available only if selecting it actually applies the
containment it names. `jobobject` and `appcontainer` MUST probe `Absent` for as
long as pane spawn does not assign the pane's process to a Job Object or an
AppContainer: reporting a boundary that is never applied is a false security
claim, and a "present" backend additionally produces a `SandboxSpec` that routes
the pane through a POSIX composer it cannot satisfy.

#### Scenario: doctor on a bare Windows box

- **WHEN** `thegn doctor` runs on native Windows with no container runtime
  installed
- **THEN** `jobobject` is reported as not available, `host` is the selected
  backend, and the "no kernel boundary" caveat is shown
