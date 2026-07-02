# Sandbox

## Purpose

Each worktree's interactive process can run inside an isolation backend so that
untrusted or experimental work is contained, while the worktree itself stays on
the host so host-side git reads keep working. Backend selection degrades
gracefully across the available container/sandbox runtimes.

## Requirements

### Requirement: Graceful backend selection

The sandbox SHALL select an isolation backend by preference order podman -> docker -> bwrap -> none, MUST fall back to the next when a runtime is unavailable, and MUST fall back to `none` (run on the host) rather than failing to launch when no backend exists.

#### Scenario: Preferred runtime missing

- **WHEN** podman is not installed but docker is
- **THEN** the worktree process launches under docker

#### Scenario: No runtime available

- **WHEN** none of podman, docker, or bwrap is available
- **THEN** the process runs with backend `none` on the host and the worktree is
  still usable

### Requirement: Worktree stays on the host and is bind-mounted

A sandboxed worktree SHALL remain on the host filesystem and MUST be bind-mounted into the container at its real host path, so host-side git reads and the compositor continue to operate on the same files.

#### Scenario: Host git reads remain coherent

- **WHEN** a worktree process runs inside a container backend
- **THEN** the worktree is bind-mounted at its real path and git status/diff read
  from the host see the same working tree the sandboxed process edits

### Requirement: Sandboxing is per-worktree

Isolation SHALL be configurable per worktree and MUST NOT be a single global setting for the whole session.

#### Scenario: Mixed backends across worktrees

- **WHEN** two worktrees are open with different sandbox settings
- **THEN** each worktree's interactive process uses its own configured backend
  independently

### Requirement: Shared .git/config is mounted read-only inside the sandbox

A sandboxed worktree process SHALL see the shared `<git-common>/config` mounted read-only while objects, refs, index, and the per-worktree `worktrees/<name>/config` stay writable, so commits work but no sandboxed process can pollute the shared config.

#### Scenario: In-sandbox commit works

- **WHEN** a sandboxed process commits
- **THEN** the writes to objects/refs/index succeed

#### Scenario: In-sandbox shared-config write is refused

- **WHEN** a sandboxed process runs `git config`/`git remote add` against the
  shared config
- **THEN** the write fails by design

### Requirement: Per-worktree tunnel via a sidecar leaves the host untouched

A worktree MAY attach to its own overlay network through a per-worktree sidecar container whose network namespace the worktree joins (`--network container:<sidecar>`); superzej MUST NOT embed a tunnel datapath, and the host's networking (including any host `tailscaled`) MUST remain unchanged.

#### Scenario: Worktree egress is the tunnel

- **WHEN** a VPN is enabled for a worktree
- **THEN** the sidecar holds NET_ADMIN/TUN, the worktree's only egress is the
  tunnel, and the host network is untouched

#### Scenario: Sidecar torn down with the worktree

- **WHEN** the worktree closes
- **THEN** the `-szvpn` sidecar is removed and the ephemeral node de-registers

### Requirement: SealedTunnel profile has no direct host egress

The `SealedTunnel` profile SHALL apply the same lockdown as `sealed` but route egress through the tunnel, MUST degrade to `network=none` when no VPN is configured, and plain `sealed` MUST refuse a VPN.

#### Scenario: No VPN degrades to offline

- **WHEN** `SealedTunnel` is selected without a VPN configured
- **THEN** the worktree runs with `network=none`

#### Scenario: Plain sealed refuses a VPN

- **WHEN** a VPN is configured under plain `sealed`
- **THEN** it is refused

### Requirement: Tunnel failure never falls through to a less-isolated backend

When a tunnel fails to come up, the `on_error` policy SHALL govern the outcome and the `fail` setting MUST abort rather than launch the worktree with weaker isolation.

#### Scenario: on_error=fail aborts

- **WHEN** the tunnel fails to become ready and `on_error=fail`
- **THEN** the worktree does not launch with direct host egress

### Requirement: Resolve and inject the repo devShell env into worktree panes

When a worktree's repo exposes a flake `devShell` and `[sandbox] inject_devshell` is enabled, superzej SHALL resolve the devShell env on the host (`nix print-dev-env --json`), cache it by a `flake.lock`+`flake.nix` hash, and merge the exported variables into each worktree pane before the sandbox exec (PATH prepended, other vars set only if unset); a repo without `nix`/`devShell` MUST be a clean no-op.

#### Scenario: Flake repo gets the toolchain

- **WHEN** a worktree pane is spawned in a repo with a flake devShell
- **THEN** the pane's PATH includes the devShell tool directories

#### Scenario: Non-flake repo is a no-op

- **WHEN** a worktree pane is spawned in a repo with no flake devShell
- **THEN** no `nix` is invoked and the pane gets its ordinary environment

### Requirement: devShell resolution runs off the event loop

The devShell resolve SHALL run on a background thread that pulses the `TerminalWaker` and writes the cache, MUST NOT block pane spawn, and MUST NOT add a polling timeout; a cold pane applies the cache on a later spawn once warm.

#### Scenario: Cold resolve does not block

- **WHEN** the devShell cache is cold at pane spawn
- **THEN** the pane spawns immediately and the resolve proceeds off-loop, applying
  to subsequent spawns

### Requirement: Opt-in nix daemon mount

`[sandbox] nix_daemon` (default false) SHALL bind-mount the nix daemon socket and set `NIX_REMOTE=daemon` so full `nix develop`/`build` work inside the sandbox, and MUST warn and stay off when the host has no daemon socket.

#### Scenario: Enabled with a host daemon

- **WHEN** `nix_daemon` is true and the host daemon socket exists
- **THEN** the sandbox mounts the socket and nix operations work inside it

#### Scenario: No host daemon

- **WHEN** `nix_daemon` is true but no host daemon socket exists
- **THEN** superzej warns and leaves the mount off rather than half-wiring nix
