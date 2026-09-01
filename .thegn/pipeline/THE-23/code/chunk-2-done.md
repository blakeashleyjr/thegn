# THE-23 chunk 2 completion

Implemented the host-owned devcontainer provider seam and integrated it with
trusted off-loop launch resolution. Provider startup is limited to configs with
approved source/gates and a clean field inventory; otherwise the existing OCI
fallback remains authoritative. Pane commands use the provider's opaque exec
adapter and the existing CPU-cap path.

Added transient, path-keyed devcontainer status to hydration, sidebar detail
tokens, the active tab-bar cluster, and the switch cache. Added repo-context
doctor text/JSON covering candidates, selection errors, provider probing, trust
categories, disposition lists, and backend honorability. Added the isolated
fixture-based smoke assertion for the doctor block.

## Verification

- `bash -n test/smoke.sh` — passed.
- `git diff --check` — passed.
- `cargo fmt --all` — completed.
- `just quick thegn-host` — started with escalation twice; compilation reached
  host-crate diagnostics. It was stopped while waiting through the long shared
  Cargo/link phase after the allowed targeted duration.
- Focused `cargo nextest` was started once for `devcontainer`; it reached the
  host compile and reported/fixed existing-test API incompatibility caused by
  adding fields to `SandboxOutcome`. The provider session was moved behind an
  internal registry so those out-of-scope test literals remain unchanged.

## Unverified

- Final host-crate link and the focused nextest filters (`devcontainer`,
  `doctor`, `switch_cache`) remain unverified because the shared build lock and
  host link exceeded the dev-loop duration budget. No e2e, migration, live-state
  binary invocation, or full-workspace gate was run.
