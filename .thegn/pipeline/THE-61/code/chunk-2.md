# Chunk 2 — sync and archive the OpenSpec change

## Files touched

Modify the existing change files:

- `openspec/changes/record-dependency-adoption-decisions/proposal.md`
- `openspec/changes/record-dependency-adoption-decisions/design.md`
- `openspec/changes/record-dependency-adoption-decisions/tasks.md`
- `openspec/changes/record-dependency-adoption-decisions/specs/architecture-gates/spec.md`
- `openspec/specs/architecture-gates/spec.md`

Then archive the completed change by moving those four change artifacts to:

- `openspec/changes/archive/2026-08-29-record-dependency-adoption-decisions/proposal.md`
- `openspec/changes/archive/2026-08-29-record-dependency-adoption-decisions/design.md`
- `openspec/changes/archive/2026-08-29-record-dependency-adoption-decisions/tasks.md`
- `openspec/changes/archive/2026-08-29-record-dependency-adoption-decisions/specs/architecture-gates/spec.md`

Do not touch `docs/adr/`, manifests, lockfiles, source code, or ratchets. These
paths are file-disjoint from Chunk 1 and the chunk can be implemented before
or after Chunk 1.

## Approach

Reconcile the draft against the checked-out branch before archiving:

- Preserve the already-landed decisions for sysinfo, Windows-rs, and
  tungstenite, plus the reject/defer rationale for rustix, whoami, and
  zerocopy.
- Remove the proposed Windows `windows-sys`/`windows` version bump and
  `deny.toml` comment edit. They are not safe documentation-only work and
  require a separate Windows target migration.
- Correct the audit description: `just deps-audit` is
  `cargo deny check` plus `cargo machete` (`justfile:455-462`), is included by
  `just ci` (`justfile:394-397`), and is run by the dedicated CI job
  (`.github/workflows/ci.yml:121-138`); `just lint` does not call it here.
- Keep one ADDED `architecture-gates` requirement, but state only the actual
  deny.toml policies and audit recipe. Sync that delta into the canonical
  `openspec/specs/architecture-gates/spec.md` before moving the change.
- Mark all OpenSpec tasks complete only after the canonical spec is synced and
  the archive has the same final artifacts. Note that no numbered roadmap item
  exists; the dependency spine is documented at `tasks.md:232-251`.

The final archived proposal/design/tasks/spec must be internally consistent
and must not claim runtime behavior, a config key, a capability, a help page,
or a migration was implemented.

## Overlap and dependency

No overlap with Chunk 1. No ordering dependency. The canonical spec is the
only shared architectural source, and the archive must preserve the exact
synced delta rather than introduce a second variant.

## Tests / validation

This chunk is OpenSpec/docs-only. No Rust test is warranted; if the standard
scoped command pair is required by the local workflow, use `just quick thegn-core`
and `cargo nextest run -p thegn-core crate_boundaries` (no source behavior is
expected to change). Run `just openspec-validate` (or the equivalent pinned
`openspec validate --all --strict`) and `git diff --check`. Do not run `just ci`,
`just test`, e2e, or a full-workspace compile in this chunk.

## Done criteria

- The canonical architecture-gates spec contains exactly the corrected synced
  requirement, with no false `just lint` claim.
- The in-flight change is absent from `openspec/changes/` and present under
  the dated archive with proposal, design, tasks, and delta spec.
- OpenSpec strict validation passes; no runtime, manifest, lockfile, config,
  help, catalog, or ratchet changes exist.
- Commit with the exact subject:

  `docs(the-61): sync and archive dependency decision`
