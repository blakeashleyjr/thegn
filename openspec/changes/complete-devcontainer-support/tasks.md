# Tasks — complete devcontainer support

## 1. Core contract

- [x] 1.1 Add deterministic primary/variant discovery, explicit selection,
      ambiguity/read/parse results, and JSONC parser fixtures.
- [x] 1.2 Add stable `${devcontainerId}` and clamp `${localEnv:NAME}` to the
      effective `sandbox.env_passthrough` allowlist without exposing values.
- [x] 1.3 Add the exhaustive recognized-field inventory for the supported
      contract: applied, refused, reserved, editor-only, and unknown.
- [x] 1.4 Reconcile trust-gated source/mount/port/lifecycle/feature folding,
      trusted-user precedence, additive values, and backend honorability.
- [x] 1.5 Add `sandbox.devcontainer = auto|off`, the top-level repo variant
      selector, config round trips, and generated-reference source comments.
- [x] 1.6 Preserve the native OCI feature planner: override ordering,
      options-to-env, in-container oras/curl fetch, and `install.sh` execution.

## 2. Host integration and visibility

- [x] 2.1 Route selection, approvals, and the effective local-env allowlist
      through the existing off-loop repo-trust resolution path.
- [x] 2.2 Add the host-owned optional CLI provider with bounded version probe,
      opaque start/exec handle, safety eligibility, and native OCI fallback.
- [x] 2.3 Keep feature and one-time lifecycle provisioning on the existing host
      OCI path; preserve per-pane postStart/postAttach behavior and CPU caps.
- [x] 2.4 Add read-only doctor text/JSON for selection, provider, status,
      pending trust, field dispositions, and backend honorability.
- [x] 2.5 Add transient path-keyed sidebar and active-tab tokens for `off`,
      `ambiguous`, `invalid`, `pending`, `ready`, and `degraded`.
- [x] 2.6 Keep the existing Tier 2 `devcontainer_e2e` coverage for live
      Dockerfile build/run, image/env/lifecycle, and compose up/exec.

## 3. Documentation and contract

- [x] 3.1 Document configuration, the untrusted-repo boundary, category TOFU,
      absolute refusal of isolation-weakening flags, lifecycle frequency,
      provider fallback, doctor, and sidebar vocabulary in sandbox help.
- [x] 3.2 Reconcile this OpenSpec change with the delivered subset and remove
      claims of complete moving-reference parity.
- [x] 3.3 Reserve fetched feature-metadata dependency ordering,
      generated-Dockerfile feature layering, and image-label metadata merging.

## 4. Scoped validation

- [ ] 4.1 Run `just quick thegn-host`. (Blocked by pre-existing chunk-2
      dead-code and needless-borrow diagnostics in `handlers/repo_trust.rs`.)
- [x] 4.2 Run `cargo nextest run -p thegn-host help`.
- [x] 4.3 Run the focused OpenSpec validation filter.
- [x] 4.4 Verify all three help ratchets and the completion-slot,
      control-schema, and env-overlay ratchets without modifying them.

Full workspace, e2e, migration, and live-state runs are intentionally excluded
from this documentation chunk. The pre-existing Tier 2 test is recorded above,
not rerun here.
