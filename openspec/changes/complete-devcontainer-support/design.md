# Design — supported devcontainer contract

## Context

THE-23 reconciles documentation with the implementation delivered in the core
and host chunks. Core owns deterministic selection, pure JSONC parsing, field
inventory, substitution policy, trust requests, sandbox folding, and the
native feature/lifecycle plan. Host owns persisted approvals, the optional
`devcontainer` CLI process boundary, OCI provisioning, doctor, and transient UI
status. No devcontainer work runs on the render loop.

## Selection and opt-out

Discovery gives the two primary files precedence:

1. `.devcontainer/devcontainer.json`
2. `.devcontainer.json`

Only when neither primary exists are sorted
`.devcontainer/<name>/devcontainer.json` variants considered. A single variant
can be selected directly. Multiple variants require the top-level repo setting
`devcontainer = "<name>"`; ambiguity or a selector miss applies no overlay and
is visible. The selector chooses a file but grants no approval for its content.

The global/effective `[sandbox] devcontainer` mode defaults to `auto`. `off`
returns before file parsing and trust lookup. This is the explicit opt-out, not
a weaker trust mode.

## Supported field inventory

The inventory is versioned with thegn and covers fields the parser recognizes;
it is not generated from the moving containers.dev reference.

| Disposition                    | Fields                                                                                                                                                                                                                                                                                                       | Behavior                                                                      |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------- |
| Applied by the supported paths | `image`; `build` (`dockerfile`, `context`, `args`, `target`); `dockerComposeFile`, `service`, `runServices`; `features`, `overrideFeatureInstallOrder`; `mounts`; `forwardPorts`; `containerEnv`, `remoteEnv`; `workspaceFolder`; all six lifecycle commands                                                 | Subject to path support, category gates, and trusted-user precedence          |
| Refused                        | `privileged`, `capAdd`, `securityOpt`, `runArgs`, `init`                                                                                                                                                                                                                                                     | Never passed to either provider; warning names the field and isolation reason |
| Reserved                       | `hostRequirements`, `portsAttributes`, `otherPortsAttributes`, `waitFor`, `userEnvProbe`, `shutdownAction`, `updateRemoteUID`, `updateRemoteUserUID`, `secrets`, `workspaceMount`, `overrideCommand`, `remoteUser`/`containerUser` beyond feature-user seeding, `dockerComposeOverrideFile`, `remoteUserUID` | Not applied; warning names the field                                          |
| Editor-only                    | `customizations`                                                                                                                                                                                                                                                                                             | Intentionally ignored without a warning                                       |
| Unknown                        | Any other top-level key                                                                                                                                                                                                                                                                                      | Not applied; warning identifies it as unknown and reserved                    |

`devcontainer.metadata` is image-label data rather than a repo JSON key. Label
inspection and merging are reserved, and no presence warning is promised
because selection and doctor do not inspect the image.

The native fold also reports representation limits: anonymous volumes and
tmpfs mounts are skipped, read-only named volumes degrade to read-write with a
warning, and compose-service mounts/ports remain governed by the compose file.
User-pinned image, backend, profile, and network choices win; additive mounts,
ports, and environment entries are retained.

## Trust and substitution

The selected repo file is attacker-authored until approved. Core builds
canonical `GatedRequest`s for `devcontainer.image`, `.build`, `.compose`,
`.mounts`, `.ports`, `.lifecycle`, and `.features`. Approvals are persisted in
the existing `repo_trust` store. A missing approval leaves that category
pending and unapplied while the worktree opens; editing its canonical request
causes it to require approval again.

Literal `containerEnv` and `remoteEnv` values apply without a category prompt,
but only valid environment-variable names are emitted. Substitution supports
workspace folder and basename forms, `${containerEnv:NAME}`,
`${localEnv:NAME}`, and stable `${devcontainerId}`. The native path keeps the
worktree mounted at its real path; its workspace substitutions use that path.
`${localEnv:NAME}` only reads names in effective
`sandbox.env_passthrough`; a blocked name becomes empty and is reported without
exposing its value. Unknown substitution expressions remain verbatim.

Refused fields never become trust requests. In particular, a user cannot
approve arbitrary runtime flags or privileged container access from a clone.

## Execution paths and lifecycle

After selection and trust evaluation, host launch has two paths:

- **CLI-ready:** for a local, unprojected worktree with an approved container
  source, no pending request, no refused/reserved/unknown key, and a successful
  bounded `devcontainer --version` probe, the host provider may run
  `devcontainer up` and enter through its opaque exec adapter.
- **Native OCI fallback:** when the CLI is absent, degraded, ineligible, or
  fails to start, the supported and approved subset remains folded onto the
  existing OCI sandbox/build/compose seams. Raw repo JSON is not handed to the
  CLI when an unsafe or unapplied field is present.

The provider is not a new sandbox backend enum and does not bypass pane CPU
caps. Non-OCI resolution cannot honor an image/build/compose source; doctor
reports the backend honorability instead of implying that the container shape
was applied.

With lifecycle approval, `initializeCommand` maps to host-side one-time
`prepare`; `onCreateCommand`, `updateContentCommand`, and `postCreateCommand`
become ordered one-time provisioning steps. Feature steps run after the
toolchain and before those lifecycle steps. `postStartCommand` and
`postAttachCommand` map to the existing per-pane `init_script`: thegn has no
separate attach event, so running before each pane shell is the explicit
per-start/attach analogue.

The native feature planner fetches OCI feature artifacts inside the container
with `oras` or a curl fallback, maps scalar options to environment variables,
seeds the supported feature user environment, and runs `install.sh`.
`overrideFeatureInstallOrder` has priority; the planner then uses its
deterministic fallback order. Fetched `installsAfter`/`dependsOn` metadata,
cycle planning, and generated-Dockerfile feature layers are reserved.

## Visibility without live builds

`thegn doctor` reads the current repo context, selection result, persisted
approvals, fully resolved worktree environment, and backend honorability. For an
explicitly uncontained local environment it reports devcontainer execution as
off without parsing the repo file. Its only
process probe is bounded `devcontainer --version`; it does not pull an image,
build, start a container, execute lifecycle code, or make a network probe. Text
output is headed `Devcontainer support` and reports `mode`, `repo`,
`candidates`, `selected`, optional `selection`, `provider`, `status`, `trust`,
and `backend`. JSON exposes the corresponding detailed lists.

Off-loop hydration computes the same transient status for each worktree from
the worktree's fully resolved environment (including workspace/worktree env
selection and its sandbox overlay). An explicitly uncontained local environment
does not read or apply repo devcontainer configuration. The sidebar shows
`dc:<selected-path> [<state>]`, or `dc:[<state>]` when no path is selected. The
active tab-bar environment cluster shows that token only when the observed
runtime backend is the devcontainer provider; repository/config status must not
masquerade as active containment. The states are:

- `off`: effective mode disabled;
- `ambiguous`: multiple variants need a selector;
- `invalid`: read, parse, or selector failure;
- `pending`: one or more trust requests are not approved;
- `ready`: the config is safe and the CLI provider is ready;
- `degraded`: no source, unsafe/unapplied fields, unavailable/degraded CLI, or
  another condition prevents CLI-ready execution; the native OCI fallback may
  still honor the supported subset.

No token is stored in SQLite; hydration and the path-keyed switch cache replace
it from current config, approvals, and probe state.

## Damage and persistence

- Parser, inventory, selection, and overlay remain substrate-free core logic.
- Parsing and provisioning stay off the UI loop; no wake source or ticker is
  added.
- Existing `repo_trust` rows are reused; no schema migration.
- Doctor is observational and performs no live build/start.
- No action, keybinding, panel context, capability-catalog row, control route,
  completion slot, or environment key is introduced.

## Explicit reservations

- Fetched feature-metadata dependency/topological ordering.
- Generated-Dockerfile feature layering.
- `devcontainer.metadata` image-label inspection or merge.
- Automatic parity with the complete or future containers.dev reference.
- A consent path for isolation-weakening fields.
