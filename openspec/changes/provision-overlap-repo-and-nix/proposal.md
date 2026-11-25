## Why

A first Sprites `env up` measured on 2026-11-25 took about 95 s to a usable
shell: repository seed 25.9 s, Install Nix 19.3 s, Install direnv 18.8 s, setup
28.5 s, all run one after another (THE-2708). The repository steps write only
the workdir and the Nix and direnv installs write only `/nix` and `$HOME`, so
waiting for one before starting the other wastes the shorter of the two.

## What Changes

- The env plan names an overlap window: the repository lane (`bundle_seed` or
  `clone`, then `git_exclude`, `local_parity`, `snapshot_restore`) and the tool
  lane right after it (`nix`, `nix_verify`, `direnv`).
- The provider applier runs the two lanes at the same time, each in its own
  order. Steps after the window start only when both lanes have finished, so
  the dev shell build still sees the seeded flake.
- Fatal steps stay fatal: a failed seed, clone or Nix check stops both lanes
  before their next step and fails the create. Best-effort failures show on
  their row as before.
- Each step still logs its own time; the window logs its wall time and the time
  the overlap saved.
