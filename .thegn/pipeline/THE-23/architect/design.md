# THE-23 — devcontainer support architecture

Status: implementation design for `tg/the-23-devcontainer`
Scope: repo-owned `.devcontainer/devcontainer.json`, `.devcontainer.json`, and
explicitly selected `.devcontainer/<name>/devcontainer.json` variants.

## Decision

Treat a devcontainer as an untrusted, repo-authored overlay of the existing
environment model. Detection, JSONC parsing, normalization, selection, field
classification, substitution, and overlay planning remain pure
`thegn-core` operations. Starting a devcontainer CLI process belongs to a new
host provider seam. The existing `SandboxSpec`/backend resolver remains the
fallback when the CLI is absent, and remains the single path used to construct
pane and agent argv.

The minimum supported contract is `image`, `build`, `features`,
`containerEnv`, `remoteUser`, `forwardPorts`, `postCreateCommand`, and
`mounts`. Existing support for the other already-normalized fields is retained
only where its current hook mapping is tested and documented. No new sandbox
backend, database table, control operation, action, or capability-catalog row
is introduced.

## Evidence and draft verification

The OpenSpec draft correctly identifies the current implementation as largely
landed: the pure parser is in `crates/thegn-core/src/devcontainer.rs:1-15`,
the trust-gated fold is in `crates/thegn-core/src/devcontainer_overlay.rs:82-145`,
native OCI feature planning is in `crates/thegn-core/src/devcontainer_features.rs:1-18`,
and host wiring is in `crates/thegn-host/src/handlers/repo_trust.rs:42-107` and
`crates/thegn-host/src/host_provision.rs:111-150`. The existing live OCI test
is Tier 2 and must not be run for this work.

The draft gaps are real. `detect` currently chooses the first variant by
lexicographic order (`devcontainer.rs:193-220`), parse/read failure is reduced
to `None` (`devcontainer.rs:222-231`), and the host supplies an unrestricted
`std::env::var` closure (`handlers/repo_trust.rs:71-83`). The overlay currently
gates source, mounts, ports, lifecycle, and features, but literal environment
values are ungated (`devcontainer_overlay.rs:82-145`); it has no shared field
inventory or refusal/reserved reporting. The current env-plan probe only checks
the two primary paths (`envplan.rs:100-115` and `:193-210`).

The draft is pruned in two places:

- Feature metadata fetching and `installsAfter`/`dependsOn` topological ordering
  are not part of this issue’s parser subset and would add network/provider
  work to a pure core planner. Preserve the existing deterministic
  override-then-declaration order in `devcontainer_features.rs:125-150` and
  reserve metadata ordering as a separately specced capability.
- “Every field in the containers.dev reference” is not a stable implementation
  boundary. This change classifies the known security-sensitive and commonly
  encountered keys, plus every key that the parser recognizes; it does not
  promise the moving reference schema. `customizations` remains editor-only;
  unsupported recognized keys are visible as reserved. Unknown keys are
  reported as unknown/reserved in doctor, never executed.

The draft’s trust model, explicit multi-config selection, `${devcontainerId}`,
local-env clamp, opt-out, non-OCI warning, doctor visibility, and no-DB/no-
catalog decisions are retained after the code check.

## Core contract

1. `devcontainer_select.rs` returns candidates and a deterministic selection
   result. The two primary paths retain their existing precedence. A variant is
   selected only by a repo selector; multiple unselected variants produce an
   ambiguity result naming all relative paths. A malformed selected file is a
   surfaced parse error with no partial overlay. `envplan` presence detection
   uses the same candidate rules, so auto-selection and the launch probe cannot
   disagree.
2. `devcontainer.rs` remains JSONC tolerant and substrate-free. Add
   `${devcontainerId}` from the repo identity plus selected relative config
   path. Keep variable expansion unknown-variable-preserving. Return a
   substitution report containing blocked `${localEnv:NAME}` reads; the host
   supplies values only when `NAME` is in the effective
   `[sandbox].env_passthrough` list. A blocked value is empty and names the
   variable in the warning; no token value is logged.
3. `devcontainer_inventory.rs` owns the one field-classification table used by
   parser, overlay, and doctor. Applied fields are the supported subset and
   tested existing mappings. `privileged`, `capAdd`, `securityOpt`, `runArgs`,
   and `init` are refused unconditionally because they can weaken the trusted
   sandbox. `hostRequirements`, port-attribute fields, `waitFor`,
   `userEnvProbe`, `shutdownAction`, `updateRemoteUID`, `secrets`,
   `workspaceMount`, unsupported user/container identity behavior, and image
   metadata are reserved with a reason. `customizations` is editor-only.
   Classification is exhaustive over the parser’s recognized-key table and
   emits one deduplicated warning per key.
4. `devcontainer_overlay.rs` continues to fold onto `SandboxConfig`, preserving
   trusted user `backend`, `profile`, `network`, and pinned image/build values;
   additive ports/mounts append only under their existing gates. All lifecycle
   and feature execution remains `GatedRequest`/TOFU. `postCreateCommand` is a
   one-time provision step. Any per-start hook already mapped to `init_script`
   is per pane, the documented honest analogue for a multiplexer attach.
   Refused fields never become requests. Add a pure backend-honourability
   result; the host supplies the resolved backend family and surfaces a warning
   when a trusted source cannot be honored.
5. Add `[sandbox] devcontainer = "auto" | "off"`, default `auto`, through the
   existing `config_enum!`/overlay/layering paths. `off` short-circuits before
   reading or trusting the repo file. Add a repo-root selector
   `devcontainer = "<variant>"` alongside the existing top-level `env` in
   `RepoConfigFile`; it chooses a file and grants no execution authority.
   The repo selector is not a sandbox backend selector and cannot opt out of
   trust gates.

The core remains free of tokio, PTY, HTTP, and vendor process code, consistent
with `docs/ARCHITECTURE.md:9-37`. New logic goes in sibling modules rather
than enlarging the already large parser/config files. Core tests use fixture
JSONC files and temporary paths only.

## Host provider and launch flow

Add `crates/thegn-host/src/devcontainer_provider.rs` with an object-safe
`DevcontainerProvider` seam. Its implementation owns `std::process::Command`,
the `devcontainer` executable discovery/version probe, CLI argv construction,
exit/status parsing, and an opaque container handle. Vendor-specific flags must
not leak into core or call sites. The provider exposes `ProbeReport` data for
doctor and a capability decision: CLI ready, CLI unavailable, or CLI degraded.
The CLI is optional; its absence selects the existing OCI path.

The launch coordinator runs the pure selection/overlay once off-loop after
repo trust resolution. When CLI-ready and a container source is selected, the
provider builds/starts it with the worktree as the workspace bind mount and
returns a handle plus a provider-owned `exec` argv adapter. Otherwise the
existing OCI backend consumes the folded `SandboxSpec`; no new `Backend` enum
member is needed. Both adapters pass through one narrow core CPU-cap wrapper
before PTY/background spawn (the OCI adapter continues through
`sandbox::enter_argv`, `sandbox.rs:1846-1863`, which already calls
`sandbox_cpucap::wrap_pane_argv`; the provider adapter reuses the same wrapper).
Agent/background handoff continues through `wrap_background_argv`
(`sandbox_cpucap.rs:483-631`). Thus panes and agents cannot accidentally escape
the selected devcontainer or the shared resource ceiling. No launch, build, CLI
probe, or filesystem scan is added to the UI loop. The existing off-loop
trust/provision paths (`repo_trust.rs` and `host_provision.rs`) carry warnings
and wake the compositor through their current notification channels.

`thegn doctor` adds a repo-context `devcontainer` object and text block with:
mode, candidates/selected file or ambiguity, parse result, provider probe,
approved/pending categories, refused/reserved keys, and effective backend
honorability. Doctor never builds, starts, or runs a lifecycle command. It may
perform the provider’s bounded executable probe. The existing provider registry
and `CATALOG` are unchanged: doctor is already an external verb and the
devcontainer provider is a host implementation seam, not a new user operation.

The sidebar/tab-bar state is transient and derived during off-loop hydration;
it is not persisted or added to SQLite. Add a small status value to the existing
worktree/frame slice and cache it with the path-keyed switch slice, then render
the existing env token as `dc:<variant> [state]` (or a terse degraded/pending
form) beside the backend token. A missing or stale status must not survive a
worktree switch. The same state is used by the active tab-bar env cluster and
sidebar detail line; no second capability catalog is created.

## Trust and failure policy

Reading and parsing is safe; repo-provided execution is not. Image/build/CLI
start, mounts, ports, lifecycle, and features retain category-specific TOFU
requests keyed by canonical substituted content. Unapproved requests remain
pending while the worktree opens. Literal `containerEnv` is container-scoped,
but substitutions can only read the user allowlist. Refused isolation flags are
never passed to either provider. `devcontainer = off` means no parse, warning,
or prompt. Missing CLI, unsupported source shape, malformed JSONC, ambiguous
variants, and non-OCI fallback are degraded states with explicit reasons; they
do not silently run a different container or host process.

## Documentation and ratchets

Document the mode and selector, supported subset, trust categories, lifecycle
mapping, local-env clamp, CLI/OCI fallback, and doctor/sidebar states in
`config/config.toml.example` and `docs/help/sandboxing.md`. The generated
config-reference remains generated; do not hand-edit it.

Ratchet disposition must be recorded in the implementation commit:

- Add `sandbox.devcontainer` to `test/env-overlay-ratchet.txt` and its env
  overlay test. This is the only new config environment key.
- Run the completion-slot ratchet; it stays unchanged because no command or
  value-taking CLI argument is added.
- Run control-schema snapshot tests; `docs/api/control-v1.json` stays unchanged
  because there is no new control operation or catalog row.
- Run all three help ratchets. They stay unchanged because this adds prose to
  an existing page, not an action/context/panel key. If the implementation adds
  an action later, that action must be documented and its ratchet updated in
  the same commit.

These choices honor the 0%-idle and provider-seam rules in
`docs/ARCHITECTURE.md:54-84` and `:110-149`, the single capability catalog in
`:151-197`, config/overlay rules in `:199-214`, and help rules in `:216-228`.

## Delivery order

The three chunks are serial: core contract first, host integration second,
documentation/spec verification third. Each is file-disjoint from the others;
dependencies are explicit in the chunk files. Coder commits use the exact
subjects specified there. Validation is deliberately scoped to `just quick
<crate>` and filtered `cargo nextest`; no full-workspace build, `just test`,
`just ci`, migration, live-state invocation, or e2e run is authorized.
