# Complete devcontainer support for the supported subset

Linear: THE-23

## Why

The devcontainer implementation predates its capability contract. It already
parses JSONC, folds a useful subset onto the sandbox model, provisions features
and lifecycle commands on OCI hosts, and has a live-runtime Tier 2 test, but a
repo author has had no single place to learn the security boundary or the
degradation behavior.

This change documents the implemented subset without promising parity with the
moving containers.dev reference. A repo-authored `devcontainer.json` is
untrusted input: executable or host-affecting categories require persisted
trust-on-first-use approval, isolation-weakening fields are refused even after
approval, and other recognized-but-unapplied fields are reported as reserved.

## What Changes

- Specify deterministic discovery of `.devcontainer/devcontainer.json`,
  `.devcontainer.json`, and explicitly selected
  `.devcontainer/<name>/devcontainer.json` variants.
- Specify the supported parser and native OCI subset: image, Dockerfile build,
  compose service, mounts, forwarded ports, container/process environment,
  lifecycle hooks, feature install scripts, and supported substitutions.
- Specify category-level TOFU requests for `image`, `build`, `compose`,
  `mounts`, `ports`, `lifecycle`, and `features`. Literal environment values
  remain ungated, but `${localEnv:NAME}` is empty and reported unless `NAME` is
  in the effective `sandbox.env_passthrough` allowlist.
- Specify the field inventory actually implemented by the parser: refused
  isolation flags, reserved recognized fields, silent editor-only
  `customizations`, and visible unknown top-level keys. This is an inventory of
  the supported contract, not a promise to mirror every future reference key.
- Specify `[sandbox] devcontainer = "auto" | "off"`, repo variant selection,
  user-config precedence, and the optional CLI-provider/native-OCI fallback.
- Specify the read-only `thegn doctor` report and the transient sidebar/tab-bar
  token states: `off`, `ambiguous`, `invalid`, `pending`, `ready`, and
  `degraded`.

## Implemented Boundaries

- Feature ordering honors `overrideFeatureInstallOrder`; remaining features
  use the native planner's deterministic fallback order. Fetching feature
  metadata to honor `installsAfter` or `dependsOn` is reserved.
- Features are fetched and installed inside an existing OCI container.
  Generating a Dockerfile to bake feature layers into an image is reserved.
- `devcontainer.metadata` image-label merging is reserved. The doctor probe
  does not inspect or pull an image, so it makes no claim that such labels are
  detected.
- The recognized-field table is deliberately versioned with thegn. Unknown
  top-level keys warn as reserved; THE-23 does not promise automatic parity
  with future containers.dev fields.

## Impact

- **Roadmap:** documents the delivered scope of tasks.md AB 358 and its
  existing Tier 2 `devcontainer_e2e` coverage.
- **Specs:** adds the `devcontainer` capability delta. The existing `sandbox`
  capability remains the execution substrate.
- **Configuration:** documents the already-landed `sandbox.devcontainer` mode
  and top-level repo `devcontainer` variant selector. The generated
  config-reference help remains sourced from `config/config.toml.example`.
- **Security/persistence:** reuses `GatedRequest`, `Approvals`, and the existing
  `repo_trust` table; no DB migration or new credential surface.
- **CLI/API:** `thegn doctor` and `thegn repo trust` carry the visibility and
  approval flow. No action, capability-catalog row, control field, completion
  slot, or environment key is added.

## Non-goals

- Mirroring the complete or future containers.dev reference.
- Fetched feature-metadata dependency ordering.
- Generated-Dockerfile feature layering.
- Image-label `devcontainer.metadata` merging.
- Applying `privileged`, `capAdd`, `securityOpt`, `runArgs`, or `init` under any
  consent flow.
