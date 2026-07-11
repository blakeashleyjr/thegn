# Design

## Resolver (thegn-core, new `devenv.rs`)

A substrate-agnostic resolver with one job: repo path → map of exported env vars.

- Run `nix print-dev-env --json` **on the host** for the repo root; extract the
  `exported` variables (primarily `PATH`, plus shellHook exports like
  `THEGN_YAZI_BIN`). JSON avoids fragile shell parsing.
- Cache at `$XDG_STATE_HOME/thegn/devenv/<hash>.json`, `<hash>` = content hash
  of `flake.lock` + `flake.nix`; any flake change invalidates.
- Degrade silently: no `nix`, no `devShell`, or non-zero → return `None`, log info.

## Off-loop + inject

- `print-dev-env` is multi-second → runs on a **background thread** (diff-watcher /
  hydration pattern), pulses `TerminalWaker`, writes the cache. Never on the loop,
  never a polling timeout. The active workspace is prewarmed at startup.
- At pane spawn, merge cached exported vars **host-side, before the sandbox exec**
  (mirrors the existing `sandbox.rs` resolve-at-spec-build). `PATH` is prepended;
  other vars set only if unset. Realized store paths are already RO bind-mounted,
  so tools run unchanged in every backend.

## Tier B daemon (fast-follow)

`[sandbox] nix_daemon` (default `false`) bind-mounts
`/nix/var/nix/daemon-socket/socket` (+ required `/nix/var/nix` paths) and sets
`NIX_REMOTE=daemon`. Host precondition: if no daemon socket exists, warn and stay
off. This is the only piece that relaxes isolation, hence opt-in.

## Config

| Key                         | Default | Effect                     |
| --------------------------- | ------- | -------------------------- |
| `[sandbox] inject_devshell` | `true`  | Tier A resolve + inject    |
| `[sandbox] nix_daemon`      | `false` | Tier B daemon socket mount |

## AI-additive / invariants

No AI involvement. Resolution never runs on the loop; non-flake repos are a clean
no-op (no `nix` invocation).
