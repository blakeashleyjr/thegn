# Chunk 2 completion — sync and archive the OpenSpec change

Implemented Chunk 2 for THE-61. The dependency-adoption OpenSpec change is
archived at
`openspec/changes/archive/2026-08-29-record-dependency-adoption-decisions/`.
The in-flight change is absent from `openspec/changes/`, and the canonical
`openspec/specs/architecture-gates/spec.md` contains the corrected dependency
audit and adoption-record requirement.

The archived artifacts preserve the six dependency decisions and explicitly
exclude the proposed Windows version bump, the `deny.toml` comment edit, and
all runtime, manifest, lockfile, config, capability, help, ratchet, and schema
changes. The audit wording identifies `just deps-audit` as `cargo deny check`
plus `cargo machete`, included by `just ci` and the dedicated CI job; `just
lint` is not described as running that audit.

## Validation

- `git diff --check` passed.
- Confirmed the in-flight OpenSpec directory is absent and all four dated
  archive artifacts are present.
- Confirmed the canonical requirement and archived delta contain the corrected
  `just deps-audit` wording with no false `just lint` claim.

## Unverified

- `just openspec-validate` could not run because `openspec` is not installed in
  the current shell (`command not found`).
- The pinned `nix run .#openspec -- validate --all --strict` equivalent could
  not run because the sandbox cannot connect to the Nix daemon socket.
- No Rust or e2e tests were run; this chunk is OpenSpec/docs-only and the
  chunk policy excludes full-workspace gates and e2e.
