# Devcontainer

## ADDED Requirements

### Requirement: Repo devcontainer selection and parsing is deterministic and non-executing

thegn SHALL give `.devcontainer/devcontainer.json` precedence over
`.devcontainer.json`, and SHALL consider sorted
`.devcontainer/<name>/devcontainer.json` variants only when neither primary
file exists. It SHALL parse the selected file as JSONC, including line and
block comments and trailing commas, without executing code, starting a
container, or performing network I/O. Lifecycle command, forwarded-port, and
mount polymorphic forms SHALL be normalized for later phases. A read or parse
failure MUST apply no partial overlay and MUST be surfaced without preventing
the worktree from opening.

#### Scenario: JSONC is normalized without side effects

- **WHEN** the selected file contains comments, a trailing comma, an integer
  `forwardPorts` entry, and an argv lifecycle command
- **THEN** parsing produces normalized values and performs no process or
  network operation

#### Scenario: Malformed JSONC is visible and applies nothing

- **WHEN** the selected file is malformed
- **THEN** its selection is reported as invalid with the parse reason, no
  devcontainer overlay is applied, and the worktree can still open

### Requirement: Variant selection is explicit when discovery is ambiguous

thegn SHALL allow a top-level repo setting `devcontainer = "<name>"` to select
a `.devcontainer/<name>/devcontainer.json` variant. The selector SHALL grant no
trust to the selected content. When multiple variants exist without a
selector, or a selector matches no candidate, thegn MUST apply none and MUST
surface the selection failure rather than guessing.

#### Scenario: Ambiguous variants apply nothing

- **WHEN** `.devcontainer/a/devcontainer.json` and
  `.devcontainer/b/devcontainer.json` exist and no selector is set
- **THEN** both candidates are reported, the status is `ambiguous`, and no
  overlay is applied

#### Scenario: A selector chooses but does not approve

- **WHEN** the repo sets `devcontainer = "b"` and variant `b` requests an image
- **THEN** variant `b` is selected and its `devcontainer.image` request remains
  pending until separately approved

### Requirement: Host-affecting categories use canonical trust-on-first-use approvals

thegn SHALL create separate canonical trust requests for `image`, `build`,
`compose`, `mounts`, `ports`, `lifecycle`, and `features`. A category without a
matching persisted approval MUST remain pending and MUST NOT apply, while
other independently approved categories may apply and the worktree may open.
Changing the canonical request SHALL require a new approval. Literal
`containerEnv` and `remoteEnv` entries SHALL apply without a category approval,
but invalid environment-variable names MUST be dropped.

#### Scenario: Lifecycle code awaits approval

- **WHEN** an unapproved repo declares `postCreateCommand`
- **THEN** no lifecycle command runs and `devcontainer.lifecycle` is reported
  as pending

#### Scenario: Editing an approved request re-prompts

- **WHEN** an approved devcontainer image value changes
- **THEN** the new canonical `devcontainer.image` request is pending and the
  changed image is not used until approved

### Requirement: Host environment substitution is allowlisted and non-secret

thegn SHALL support workspace folder and basename substitutions,
`${containerEnv:NAME}`, stable `${devcontainerId}`, and
`${localEnv:NAME}`. Unknown substitution expressions SHALL remain verbatim.
`${localEnv:NAME}` MUST read the host value only when `NAME` appears in the
effective `sandbox.env_passthrough`; otherwise the result SHALL be empty and
the variable name, but never its value, SHALL be reported. The native path
SHALL use the path-preserving worktree mount for its workspace substitutions.

#### Scenario: A blocked local variable cannot cross into the container

- **WHEN** `containerEnv.TOKEN` is `${localEnv:GH_TOKEN}` and `GH_TOKEN` is not
  in effective `sandbox.env_passthrough`
- **THEN** `TOKEN` receives an empty value and diagnostics name `GH_TOKEN`
  without revealing its host value

#### Scenario: A devcontainer identifier is stable per selected path

- **WHEN** the same repo root and selected config path are resolved in two
  sessions
- **THEN** `${devcontainerId}` has the same value in both sessions

### Requirement: The native fallback applies the supported container subset with user precedence

For an approved container source, the native OCI path SHALL support a pullable
`image`, a Dockerfile `build` with context/args/target, or a compose file and
service/run-services selection. It SHALL fold supported mounts, forwarded
ports, environment, lifecycle, and features onto the existing sandbox and host
provisioning seams. Trusted user configuration MUST retain precedence for
image, backend, profile, and network; mounts, ports, and environment are
additive. A non-OCI backend MUST NOT claim to honor an image/build/compose
source, and its backend honorability MUST be visible.

#### Scenario: A trusted user image wins

- **WHEN** trusted config pins an image and an approved devcontainer declares
  another image
- **THEN** the trusted user image remains effective

#### Scenario: A non-OCI backend does not claim a container source

- **WHEN** an approved devcontainer image resolves with a bwrap or host-family
  backend
- **THEN** doctor reports degraded backend honorability rather than reporting
  the container source as honored

### Requirement: Lifecycle frequency uses existing one-time and per-pane hooks

With lifecycle approval, thegn SHALL map `initializeCommand` to the host-side
one-time prepare hook. It SHALL run `onCreateCommand`,
`updateContentCommand`, and `postCreateCommand` as ordered one-time container
provisioning steps. It SHALL map `postStartCommand` and `postAttachCommand` to
the existing per-pane `init_script`, executed before each pane shell, because
the multiplexer has no distinct attach event.

#### Scenario: postCreate is one-time and postStart is per pane

- **WHEN** an approved config declares both `postCreateCommand` and
  `postStartCommand`
- **THEN** postCreate participates in the container's one-time provisioning
  sequence and postStart runs through init_script for each pane shell

### Requirement: Native feature installation has bounded ordering claims

With `devcontainer.features` approved, thegn SHALL plan enabled feature OCI
artifacts after toolchain provisioning and before one-time lifecycle commands,
map scalar feature options to the install environment, fetch in-container with
`oras` or the curl fallback, and invoke `install.sh`.
`overrideFeatureInstallOrder` SHALL take priority over the planner's
deterministic fallback order. Fetched `installsAfter`/`dependsOn` metadata,
dependency topological sorting, cycle handling, and generated-Dockerfile
feature layering SHALL remain reserved and MUST NOT be represented as
implemented behavior.

#### Scenario: Explicit feature override order takes priority

- **WHEN** two enabled features are named in reverse order by
  `overrideFeatureInstallOrder`
- **THEN** their native install steps follow the override order

#### Scenario: Metadata dependency ordering is not promised

- **WHEN** a feature artifact's fetched metadata declares `installsAfter`
- **THEN** thegn makes no dependency-order guarantee beyond explicit override
  order and its deterministic fallback

### Requirement: Unsafe, reserved, editor-only, and unknown fields have distinct outcomes

thegn MUST refuse `privileged`, `capAdd`, `securityOpt`, `runArgs`, and `init`
without creating an approval path or passing them to the CLI provider. It SHALL
report recognized reserved repo fields, including `hostRequirements`, port
attributes, `waitFor`, `userEnvProbe`, `shutdownAction`, remote-UID settings,
`secrets`, `workspaceMount`, `overrideCommand`, compose override, and user
settings beyond feature-user seeding. It SHALL ignore editor-only
`customizations` without warning and SHALL report any other unknown top-level
key as unknown and reserved. This inventory SHALL describe the fields known to
this thegn version and MUST NOT promise automatic parity with the complete or
future containers.dev reference.

Image-label `devcontainer.metadata` inspection and merging SHALL remain
reserved. Because selection and doctor do not inspect images, thegn MUST NOT
claim to detect that label.

#### Scenario: Privileged cannot be approved

- **WHEN** a devcontainer declares `privileged: true`, even alongside approved
  categories
- **THEN** privileged mode is not applied or passed to the CLI and the field is
  reported as refused for weakening isolation

#### Scenario: Unknown fields do not silently execute

- **WHEN** a selected file contains an unrecognized top-level field
- **THEN** the field is not applied and is reported as unknown and reserved

#### Scenario: Editor customization stays silent

- **WHEN** a selected file contains `customizations.vscode`
- **THEN** it is ignored without a warning

### Requirement: Auto mode uses a safe CLI provider with native OCI fallback

`[sandbox] devcontainer` SHALL accept `auto` and `off`, defaulting to `auto`.
`off` MUST short-circuit before parsing or trust lookup. In `auto`, a local,
unprojected config MAY be CLI-ready only when its container source and all
requests are approved, it contains no refused/reserved/unknown field, and the
bounded provider version probe succeeds. Raw repo JSON MUST NOT reach the CLI
when those safety conditions fail. If the CLI is unavailable, degraded,
ineligible, or fails to start, thegn SHALL retain the native OCI fallback for
the supported approved subset.

#### Scenario: Off bypasses the repo file

- **WHEN** effective `[sandbox] devcontainer = "off"`
- **THEN** the file is not parsed or trust-queried and status is `off`

#### Scenario: Reserved content prevents raw CLI execution

- **WHEN** an otherwise approved config contains `hostRequirements`
- **THEN** the CLI provider does not receive the raw config and the native path
  may apply only the supported approved subset

#### Scenario: CLI startup failure degrades to the native path

- **WHEN** a CLI-ready provider fails during `devcontainer up`
- **THEN** thegn surfaces the failure and continues through the existing OCI
  fallback instead of executing on the host without isolation

### Requirement: Doctor and chrome expose the same transient state without a live build

For a repo context, `thegn doctor` SHALL report the `Devcontainer support`
block with mode, candidates, selected path or selection error, provider probe,
status, pending trust and field dispositions, and backend honorability. The
probe MUST NOT pull an image, build, start a container, execute lifecycle code,
or perform network I/O; it MAY run bounded `devcontainer --version`.

Off-loop hydration SHALL expose the same transient status in sidebar and
active tab-bar tokens as `dc:<selected-path> [<state>]`, or `dc:[<state>]`
without a selected path. Supported states SHALL be `off`, `ambiguous`,
`invalid`, `pending`, `ready`, and `degraded`; no status SHALL be persisted in
SQLite. `ready` SHALL mean CLI-ready, while `degraded` MAY still use the native
OCI fallback for the supported subset.

#### Scenario: Doctor does not build to answer status

- **WHEN** doctor examines a selected Dockerfile devcontainer
- **THEN** it reports selection, trust, provider, and backend state without
  building the Dockerfile or starting a container

#### Scenario: Pending status is consistent across surfaces

- **WHEN** a selected devcontainer has an unapproved category
- **THEN** doctor reports the pending request and chrome shows a `pending`
  devcontainer token derived from current approvals
