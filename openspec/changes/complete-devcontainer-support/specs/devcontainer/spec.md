# Devcontainer

## ADDED Requirements

### Requirement: A repo devcontainer.json is detected and parsed as JSONC with normalized polymorphic fields

thegn SHALL detect `.devcontainer/devcontainer.json` and `.devcontainer.json`
at a worktree root, parse the file as JSONC (line/block comments and trailing
commas stripped to strict JSON), and normalize the spec's polymorphic fields —
lifecycle commands (`string | [argv] | {name: cmd}`), `forwardPorts`
(`int | "host:container"`), and `mounts` (shorthand string | object) — into one
canonical shape. Parsing MUST be pure (no execution, no network). A file that
fails to parse MUST surface a warning naming the parse error and apply no
overlay — never a crash and never a silent half-parse.

#### Scenario: JSONC with comments and trailing commas parses

- **WHEN** a repo ships a `devcontainer.json` containing `//` comments and a
  trailing comma
- **THEN** the file parses and its fields are available to the overlay

#### Scenario: A malformed file warns and is ignored

- **WHEN** the `devcontainer.json` is not valid JSONC
- **THEN** a warning names the parse error, no overlay is applied, and the
  worktree still opens

### Requirement: Multi-config layouts are discovered and selected explicitly

thegn SHALL discover `.devcontainer/<folder>/devcontainer.json` variant
layouts. A repo-scoped selector (`.thegn.toml` `devcontainer = "<folder>"`)
picks one variant. When more than one config exists and no selector is set,
thegn MUST warn (naming the candidates) and apply none — ambiguity is never
resolved by guessing.

#### Scenario: Two variants without a selector apply nothing

- **WHEN** a repo has `.devcontainer/a/devcontainer.json` and
  `.devcontainer/b/devcontainer.json` and no selector
- **THEN** a warning names both candidates and no devcontainer overlay is
  applied

#### Scenario: The selector picks a variant

- **WHEN** the repo's `.thegn.toml` sets `devcontainer = "b"`
- **THEN** `.devcontainer/b/devcontainer.json` is the config that overlays

### Requirement: The devcontainer overlay is trust-gated by category

The overlay SHALL gate each category — `image`, `build`, `compose`, `mounts`,
`ports`, `lifecycle`, `features` — as a `GatedRequest` through the same
repo-trust trust-on-first-use flow as a `.thegn.toml` overlay. An unapproved
category MUST NOT be applied; it is surfaced as pending and the worktree still
opens. Approval is matched by the request's canonical form, so an edited
devcontainer.json re-prompts. `containerEnv`/`remoteEnv` are literal
container-scoped values (they grant nothing repo code inside the container
does not already have) and apply ungated.

#### Scenario: Unapproved lifecycle commands do not run

- **WHEN** a worktree opens on a repo whose devcontainer declares
  `postCreateCommand` with no recorded approval for the lifecycle category
- **THEN** no lifecycle command runs, the request is surfaced as pending, and
  the worktree opens

#### Scenario: An approved category applies on the next launch

- **WHEN** the user approves the pending `devcontainer.image` request
- **THEN** the image applies at the next worktree launch without re-prompting

### Requirement: User-pinned sandbox values take precedence over the devcontainer

A sandbox value pinned by trusted config (global, profile, workspace, or a
selected `[env.<name>]`) SHALL win over the devcontainer's: the overlay only
fills unset gaps and appends to additive lists (mounts, ports, env), and MUST
NOT override the user's hardening `profile`, `backend`, or `network`.

#### Scenario: A user-pinned image is kept

- **WHEN** the trusted sandbox config pins `image` and the devcontainer
  declares a different one
- **THEN** the user's image is used

### Requirement: Lifecycle commands map onto thegn's hook points

With the lifecycle category approved, thegn SHALL map `initializeCommand` to
the host-side one-time prepare hook, `onCreateCommand` →
`updateContentCommand` → `postCreateCommand` to ordered one-time provisioning
steps run in the container, and `postStartCommand`/`postAttachCommand` to the
per-pane `init_script` (a multiplexer has no separate attach; per-pane is the
honest analogue, and the mapping MUST be documented as such).

#### Scenario: One-time versus per-pane execution

- **WHEN** a trusted devcontainer declares `postCreateCommand` and
  `postStartCommand`
- **THEN** `postCreateCommand` runs once at container creation and
  `postStartCommand` runs before each pane's shell

### Requirement: Features install natively in-container with honest ordering

With the features category approved, thegn SHALL resolve `features` as OCI
artifacts fetched inside the container (oras with curl fallback), pass options
as the spec's env contract, and execute each feature's `install.sh`. Install
order SHALL honour `overrideFeatureInstallOrder` first, then
`installsAfter`/`dependsOn` from fetched feature metadata when available, then
declaration order. Build-time feature layering (the reference CLI's generated
Dockerfile) is reserved, and MUST be surfaced as such when a feature requires
it rather than silently downgraded.

#### Scenario: Override order wins

- **WHEN** two features are declared and `overrideFeatureInstallOrder` lists
  them in reverse declaration order
- **THEN** they install in the override order

### Requirement: Variable substitution is complete and localEnv is clamped to the passthrough allowlist

thegn SHALL substitute the spec's variables (`${localWorkspaceFolder}`,
`${containerWorkspaceFolder}`, their `Basename` forms, `${localEnv:VAR}`,
`${containerEnv:VAR}`, `${devcontainerId}`), leaving unknown variables
verbatim. `${devcontainerId}` SHALL be a stable identifier derived from the
repo root and config path, identical across sessions. `${localEnv:VAR}` MUST
resolve only variables on the effective `[sandbox] env_passthrough` allowlist;
any other variable resolves to empty and a warning names it — a repo file must
not be able to copy arbitrary host env (tokens) into the container.

#### Scenario: devcontainerId is stable

- **WHEN** the same worktree's devcontainer is resolved in two sessions
- **THEN** `${devcontainerId}` substitutes to the same value both times

#### Scenario: A non-allowlisted localEnv read is refused

- **WHEN** a devcontainer sets
  `containerEnv = { T = "${localEnv:SOME_SECRET}" }` and `SOME_SECRET` is not
  in `env_passthrough`
- **THEN** the value substitutes to empty and a warning names `SOME_SECRET`

### Requirement: Every recognized-but-unapplied field is classified, never silently eaten

thegn SHALL classify every devcontainer.json field it does not apply:

- **Refused by design** (isolation-weakening): `privileged`, `capAdd`,
  `securityOpt`, `runArgs`, `init` — never applied, not even behind trust
  approval, surfaced with a warning naming the key and the reason.
- **Reserved** (recognized, not yet honoured): `hostRequirements`,
  `portsAttributes`/`otherPortsAttributes`, `waitFor`, `userEnvProbe`,
  `shutdownAction`, `updateRemoteUserUID`, `secrets`, `workspaceMount`,
  `overrideCommand`, `remoteUser`/`containerUser` beyond seeding feature
  installs, and the `devcontainer.metadata` image label — each surfaced as a
  one-line warning naming the key.
- **Editor-only** (`customizations`) — silently dropped, per the spec's
  intent.

A field in none of these classes MUST be applied; silence is never an outcome
for a recognized key outside the editor-only class.

#### Scenario: privileged is refused even when trusted

- **WHEN** a fully trust-approved devcontainer declares `privileged: true`
- **THEN** the container is not privileged and a warning states the key is
  refused by design

#### Scenario: A reserved key warns once

- **WHEN** a devcontainer declares `hostRequirements`
- **THEN** a one-line warning names `hostRequirements` as reserved

#### Scenario: customizations stay silent

- **WHEN** a devcontainer carries `customizations.vscode` settings
- **THEN** no warning is emitted for them

### Requirement: Backend interplay is visible and the overlay has an opt-out

A new `[sandbox] devcontainer = "auto" | "off"` key (default `auto`,
documented in `config/config.toml.example`) SHALL control the overlay; `off`
ignores the file entirely (one notice, no per-key warnings). When a trusted
devcontainer declares a container source (image/build/compose) and the
effective sandbox backend family is not OCI, thegn MUST surface a warning
naming the effective backend instead of silently dropping the container shape.

#### Scenario: Off means off

- **WHEN** `[sandbox] devcontainer = "off"` and the repo ships a
  devcontainer.json
- **THEN** the file is not applied and no trust prompt is raised

#### Scenario: A non-OCI backend is named

- **WHEN** a trusted devcontainer declares an image and the effective backend
  is bwrap
- **THEN** a warning states the image is not honoured because the backend is
  bwrap

### Requirement: thegn doctor reports the devcontainer state

`thegn doctor` SHALL include a devcontainer section for a repo context:
presence and which config was selected, parse result, per-category trust state
(approved/pending), refused and reserved keys found in the file, and whether
the effective backend can honour the declared container source.

#### Scenario: Doctor surfaces pending trust and reserved keys

- **WHEN** `thegn doctor` runs against a worktree whose devcontainer has an
  unapproved `mounts` category and a `hostRequirements` key
- **THEN** the output lists `mounts` as pending and `hostRequirements` as
  reserved
