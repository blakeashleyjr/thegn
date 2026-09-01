# Tasks

- [x] 1. Verify the six candidates against current manifests, lockfile,
     call sites, architecture standards, and target lanes.
- [x] 2. Publish `docs/adr/index.md` and one ADR for each requested
     crate/family.
- [x] 3. Sync the dependency-audit requirement into the canonical
     `openspec/specs/architecture-gates/spec.md`.
- [x] 4. Validate the documentation-only change and archive this OpenSpec
     change under the dated archive directory.

No dependency migration, version update, runtime behavior change, or full
workspace/e2e run is part of these tasks. The final architect commit contains
the ADRs, pipeline design/chunks, synced canonical spec, and archived change.
