# Resolve build Git identity watches

THE-575, related to THE-614. Build identity must follow the checkout being built,
including linked worktrees, without a permanently missing Cargo watch.

## Impact

Developer workflow and build diagnostics; roadmap A development/build tooling.
No runtime state, user Git configuration, or shared-target provenance changes.
