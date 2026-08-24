# Sandbox

## ADDED Requirements

### Requirement: Mise toolchains inject into worktree panes

When a worktree's repo declares a mise/asdf toolchain (any detected mise
config or pin file) and `[toolchain.mise] inject` is not `off`, thegn SHALL
prepend mise's shims directory to each worktree pane's PATH at the pane-spawn
compose seam. The shims slot MUST sit below env-bundle-set PATH entries and
below the repo devshell inject slot, and above the curated base PATH — a
flake-pinned tool is never shadowed by a mise shim. When no mise binary
exists on the host, injection MUST degrade to a clean no-op surfaced by
doctor, never an error at spawn.

#### Scenario: A pinned repo gets its tools on PATH

- **WHEN** a pane spawns in a worktree with a `mise.toml` and mise is
  installed on the host
- **THEN** the pane's PATH contains the mise shims directory below any
  devshell-injected entries

#### Scenario: No host mise is a quiet no-op

- **WHEN** a pane spawns in the same worktree on a host without mise
- **THEN** the pane spawns normally with no mise PATH entry and doctor reports
  why injection is inactive

### Requirement: Host-side mise env resolution is trust-gated and off the event loop

In `env` (or approved `auto`) mode, thegn SHALL resolve the repo's full mise
environment on the host (`mise env`), off the event loop with a waker pulse,
cached by a content hash over the detected mise config files; a cold cache
MUST NOT block a pane spawn (the pane opens with shims only and later spawns
apply the warm env). Because host-side resolution executes repo-authored
config (`[env] _.source`, templates), it MUST be gated as a trust-on-first-use
request per repo config set: unapproved, it is surfaced as pending and not
resolved. thegn MUST run `mise trust` only for a config the user has approved
in thegn — never unconditionally. Applied `[env]` values MUST fill unset gaps
only (bundle-set values are never overridden) with credential-like keys
(`*_TOKEN`/`*_KEY`/`*_SECRET`/`*_PASSWORD`) dropped, and the resolved
environment MUST NOT be persisted to config, DB, or logs.

#### Scenario: Unapproved mise env is not resolved on the host

- **WHEN** a pane spawns in a cloned repo whose `mise.toml` declares
  `[env] _.source = "./setup.sh"` with no recorded approval
- **THEN** no mise resolution runs on the host, the request is surfaced as
  pending, and the pane spawns with shims injection only

#### Scenario: mise env cannot override a bundle or inject a credential

- **WHEN** an approved repo's mise `[env]` sets `FOO` and `AWS_SECRET_KEY`,
  and the worktree's bundle already sets `FOO`
- **THEN** the bundle's `FOO` wins and `AWS_SECRET_KEY` is dropped

#### Scenario: A config edit re-prompts

- **WHEN** an approved `mise.toml` is edited
- **THEN** the changed config set is pending again and host-side resolution
  stops until re-approved

### Requirement: Toolchain provisioning precedence is fixed when nix and mise coexist

When a repo declares both a nix environment (flake/devenv/classic) and mise
config, provisioning SHALL use the nix tier — mise files never demote a
declared devshell — while `[toolchain] mode = "mise"` continues to force the
mise tier for repos declaring only language manifests. The effective layering
MUST be reportable (which tier provisioned, which injection layers applied,
in what order).

#### Scenario: A flake outranks a mise.toml

- **WHEN** a repo ships both `flake.nix` (with a devShell) and `mise.toml`
- **THEN** provisioning reproduces the devShell and the mise tier is not used
  for provisioning

### Requirement: Mise config discovery matches mise's own precedence chain

Detection SHALL recognize mise's project-level config surface —
`mise.toml`, `.mise.toml`, `mise.local.toml`, `mise/config.toml`,
`.mise/config.toml`, `.config/mise.toml`, `.config/mise/config.toml`,
`conf.d/*.toml`, `MISE_ENV` variants (`mise.<env>.toml`), `.tool-versions`,
and the idiomatic pin files (`.nvmrc`, `.node-version`, `.python-version`,
`.ruby-version`, `.go-version`, `.java-version`) — and the local filesystem
detector and the remote detection probe script MUST recognize the same set
(they are extended together).

#### Scenario: A local-override layout is detected

- **WHEN** a repo declares only `.config/mise/config.toml`
- **THEN** the worktree is detected as mise-managed both locally and via the
  remote probe

### Requirement: Doctor reports the mise toolchain state

`thegn doctor` SHALL report: mise binary presence and version, the detected
mise config files for the repo context, the effective inject mode, the trust
state of the repo's mise config, and the reason when injection is degraded
(no binary, unapproved config, inject off). Degradations are reported, never
silent.

#### Scenario: Doctor explains a degraded injection

- **WHEN** `thegn doctor` runs in a mise-managed worktree with
  `inject = "auto"` and an unapproved config
- **THEN** the output shows shims active, env resolution pending approval,
  and the mise version in use
