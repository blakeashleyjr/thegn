# Sandbox

## RENAMED Requirements

- FROM: `### Requirement: Shared .git/config is mounted read-only inside the sandbox`
- TO: `### Requirement: Git control files are read-only in every mode and on every backend`

## MODIFIED Requirements

### Requirement: Git control files are read-only in every mode and on every backend

The sandbox SHALL bind `<common>/hooks`, `<common>/config`, `<common>/info`,
`<admin>/config.worktree`, `<admin>/commondir`, `<admin>/gitdir` and
`<worktree>/.git` read-only in every `[sandbox.git]` mode, in the
`mode = "full"` structural fallback and for remote placements, on bwrap
(`--ro-bind`), OCI (`:ro`) and systemd (`ReadOnlyPaths=`), while the paths a
commit needs (objects, the lane's refs, the index) stay writable. A pin target
that is a symlink or the wrong type, or one that cannot be created, SHALL make
the shared git dir read-only for that launch. For profile `standard` and above
the existing `.claude/`, `.mcp.json`, `.envrc` and `.vscode/tasks.json` of a
local worktree SHALL be bound read-only after the worktree bind.

#### Scenario: In-sandbox commit works

- **WHEN** a sandboxed process commits on its lane branch
- **THEN** the writes to objects/refs/index succeed

#### Scenario: In-sandbox shared-config write is refused

- **WHEN** a sandboxed process runs `git config`/`git remote add` against the
  shared config
- **THEN** the write fails by design

#### Scenario: systemd pins the control files

- **WHEN** a systemd sandbox opens a linked worktree under `commit-only`
- **THEN** the unit carries `ReadOnlyPaths=` for each control pin and, when the
  common dir is read-only, `ReadWritePaths=` only for the writable carve-outs

#### Scenario: The full fallback keeps hooks read-only

- **WHEN** a `mode = "full"` pane opens a main checkout
- **THEN** `hooks`, `config` and `info` under the git dir are read-only

#### Scenario: Open does not pin host-executed files

- **WHEN** an `open` profile pane opens a worktree with a `.envrc`
- **THEN** `.envrc` is not bound read-only
