# Sandbox

## ADDED Requirements

### Requirement: Mise declarations activate through an explicit provider policy

For a detected mise/asdf declaration, `[toolchain.mise] inject` SHALL select
`auto`, `shims`, `env`, or `off`. Shims SHALL be available without evaluating
repository configuration; `auto` and `env` SHALL use the resolved environment
only after its current config-set identity is approved, otherwise degrading to
shims. `off` SHALL contribute no activation layer.

#### Scenario: Unapproved auto mode stays safe

- **WHEN** a repository has mise config but its current identity is unapproved
- **THEN** a launch may add mise shims but does not run or consume `mise env`

#### Scenario: Off means no layer

- **WHEN** `inject = "off"`
- **THEN** mise contributes no PATH or environment values

### Requirement: Mise trust is content-bound and owned by Thegn

Host/target `mise env` resolution and explicit `mise install` SHALL require an
approved `mise.env` gated request derived from the canonical detected config
set and `mise.lock` contents. A changed input SHALL invalidate approval and
cache use. Thegn MUST NOT invoke `mise trust`; explicit installation SHALL run
only `mise install` after revalidating the target identity and approval.

#### Scenario: An edit reopens the gate

- **WHEN** an approved `mise.toml` or `mise.lock` changes
- **THEN** environment resolution and installation are refused until the new
  content identity is approved

#### Scenario: Thegn does not widen ambient mise trust

- **WHEN** an operator approves the Thegn request and installs the toolchain
- **THEN** Thegn invokes `mise install` without invoking `mise trust`

### Requirement: Local and remote detection agree

Local detection and the remote detection probe SHALL recognize the same
bounded project configuration, `conf.d`, `MISE_ENV`, `.tool-versions`, and
language-pin surface, and SHALL expose the detected file list for identity and
diagnostics.

#### Scenario: A nested config is detected remotely

- **WHEN** a target worktree contains only `.config/mise/config.toml`
- **THEN** both local and remote detection classify it as mise-managed and
  include that relative path in the identity

### Requirement: Launch activation never waits for mise

Mise detection/resolution and target probes SHALL run off the event loop and
write identity-stamped owner-only cache files. Pane launch SHALL perform no
mise child process or remote call; a cold/mismatched cache SHALL return shims
or reserved safe-base state and schedule an off-loop refresh whose completion
pulses the existing refresh/waker path.

#### Scenario: A cold remote cache cannot delay a pane

- **WHEN** a remote worktree has no cached toolchain facts
- **THEN** launch uses the safe base environment while detection is scheduled
  outside the launch/event loop

### Requirement: Mise composes below trusted user and devshell layers

Activation SHALL order PATH layers as bound bundle, repo devshell, mise, then
curated base. Mise environment variables SHALL fill unset keys only and SHALL
discard credential-like keys such as `*_TOKEN`, `*_KEY`, `*_SECRET`, and
`*_PASSWORD`.

#### Scenario: A bundle value wins

- **WHEN** a bundle and approved mise environment both set `FOO`
- **THEN** the bundle value is retained, while credential-shaped mise values
  are omitted

### Requirement: Mise diagnostics are cache-safe and explanatory

Doctor SHALL report the provider, provisioning tier, injection mode, provider
state, trust state, detected files, shims, and degradation reason using bounded
presence/cached facts. It MUST NOT execute repo-authored resolution merely to
produce diagnostics.

#### Scenario: Pending trust is visible

- **WHEN** an unapproved mise config is selected in auto mode
- **THEN** doctor reports shims/safe fallback and the pending trust reason
