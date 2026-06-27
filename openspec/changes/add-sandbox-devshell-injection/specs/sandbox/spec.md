# Sandbox

## ADDED Requirements

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
