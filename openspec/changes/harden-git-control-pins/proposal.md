# Harden git control pins on every backend

## Why

Only bwrap and OCI honoured the read-only pins of the git write policy, and
only for the planned `commit-only` family. On systemd the pins were dropped, the
`mode = "full"` fallback and remote placements left the common dir read-write
with only `config` pinned, and the files the host later executes from a
worktree were writable. A sandboxed process could plant a hook, repoint
`commondir`/`gitdir`, or edit `.envrc` and have the host run it (THE-2147,
WS11-P3-01).

## What Changes

- `git_policy::control_pins` names the seven control pins (`<common>/hooks`,
  `<common>/config`, `<common>/info`, `<admin>/config.worktree`,
  `<admin>/commondir`, `<admin>/gitdir`, `<worktree>/.git`); every mode of
  `plan_git_mounts` and the legacy (`mode = "full"` fallback and remote)
  mounts append them. `pin_target_ok` fails closed on a symlinked target.
- `host_exec_paths` lists the worktree files the host executes (`.claude/`,
  `.mcp.json`, `.envrc`, `.vscode/tasks.json`); the existing ones are pinned
  read-only after the worktree bind for profile `standard` and above.
- systemd emits `ReadOnlyPaths=` for every git control pin and host-executed
  pin, and `ReadOnlyPaths=<common>` plus `ReadWritePaths=` for the policy's
  writable carve-outs when the common dir is read-only (a repo outside `$HOME`).
- The queue agent's own `pin_git_control_read_only` is removed; the core plan
  covers it.

## Impact

- Code: `thegn-core` `config_sandbox::git_policy`, `host_exec_paths`,
  `sandbox`; `thegn-host` `agent_run`.
- Docs: `docs/help/sandbox-backends.md` ("Git write policy").
- Residual: `mode = "full"` still lets a sandbox move refs; files created
  fresh by the sandbox are not pinned; `.vscode/tasks.json` can be renamed via
  its parent; Landlock-only cannot pin. `standard` is an accident guard, not a
  boundary against a hostile agent.
