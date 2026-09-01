# THE-23 chunk 3 — help and OpenSpec contract

## Files touched

- `docs/help/sandboxing.md`
- `openspec/changes/complete-devcontainer-support/proposal.md`
- `openspec/changes/complete-devcontainer-support/design.md`
- `openspec/changes/complete-devcontainer-support/tasks.md`
- `openspec/changes/complete-devcontainer-support/specs/devcontainer/spec.md`

Do not touch implementation files or ratchet files in this chunk.

## Approach

Reconcile the existing OpenSpec draft with the code contracts delivered by
chunks 1 and 2. Mark the already-landed parser, trust-gated fold, native OCI
feature planner, host provisioning path, and existing Tier 2 test as satisfied.
Remove or explicitly reserve claims that exceed this issue: fetched feature
metadata ordering/topological dependencies, generated Dockerfile feature
layering, image-label metadata merging, and a promise to mirror the entire
moving containers.dev reference. Keep the supported subset, explicit variant
selection, refusal/reserved inventory, localEnv clamp, opt-out, provider
fallback, doctor, and sidebar behavior precise.

Update sandboxing help with configuration examples and the security contract:
repo JSON is untrusted, category approvals are TOFU, refused isolation flags
are never honored, postCreate is one-time, and per-start behavior is the
existing per-pane analogue. Explain CLI-ready versus OCI fallback and the
doctor/sidebar state vocabulary. Do not hand-edit generated config-reference
help.

## Dependency / overlap

Serial after chunks 1 and 2. This chunk is file-disjoint from both and has no
implementation ownership. It must use the final public names and actual doctor
output rather than restating the draft.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host help`
- `cargo nextest run -p thegn-core openspec` (or the repository’s focused
  OpenSpec/spec validation filter)

Run the three help ratchets and OpenSpec validation as scoped checks only. Do
not run `just ci`, a full workspace build, e2e, migrations, or a live-state
binary. The completion-slot, control-schema, and env-overlay ratchets must be
verified unchanged from chunks 1/2; no new action, catalog row, control field,
or env key is introduced here.

## Done criteria

- Help and OpenSpec describe exactly the implemented subset and degradation
  behavior, including the security boundary and no-live-build doctor probe.
- Draft claims already satisfied are identified and over-broad claims are
  pruned/reserved rather than left as untestable promises.
- Generated help/config artifacts are not hand-edited and all focused checks
  pass.
- Commit exactly as: `docs(the-23): document devcontainer contract`
