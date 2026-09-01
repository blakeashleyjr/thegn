# Chunk 1 — publish the six dependency ADRs

## Files touched

- `docs/adr/index.md`
- `docs/adr/0001-rustix.md`
- `docs/adr/0002-windows-rs.md`
- `docs/adr/0003-whoami.md`
- `docs/adr/0004-sysinfo.md`
- `docs/adr/0005-zerocopy.md`
- `docs/adr/0006-tokio-tungstenite.md`

Do not touch manifests, lockfiles, source code, ratchets, OpenSpec files, or
`.thegn/pipeline` artifacts. These paths are file-disjoint from Chunk 2 and
the chunk can be implemented before or after Chunk 2.

## Approach

Create one stable ADR per candidate and an index. Each ADR must include:

1. status (`adopt`, `reject`, or `defer`) and date/context;
2. exact current workspace usage with `Cargo.toml`, `Cargo.lock`, and source
   file:line citations;
3. benefit and why it is or is not material to thegn;
4. whether it replaces an existing dependency or adds one;
5. binary-size/build cost, MSRV 1.89, Linux-musl, macOS, and mingw/Windows
   implications; and
6. unsafe/maintenance implications plus a bounded migration/reopen sketch.

Use the decisions and evidence in
`.thegn/pipeline/THE-61/architect/design.md`. State clearly that
`windows-rs`, `sysinfo`, and `tokio-tungstenite` are already adopted, and that
the Windows version alignment is deferred to a separate cross-target update.
Do not describe transitive `rustix` or `zerocopy` lock entries as direct use.

## Overlap and dependency

No overlap with Chunk 2. No ordering dependency. Chunk 1 is documentation-only
and must not add a “decision record” runtime surface, config key, help page,
capability row, or ratchet entry.

## Tests / validation

No Rust behavior changes require `just quick` or nextest. The scoped checks are
intentionally not run for this docs-only chunk; if the local workflow requires
the standard scoped commands, use `just quick thegn-core` and
`cargo nextest run -p thegn-core dependency_adoption` (there is no expected
matching test). Run `git diff --check` and manually verify every ADR link and
file:line citation. Do not run e2e or a full-workspace build.

## Done criteria

- The index links exactly six ADRs and records the verdict for each requested
  crate/family.
- Every ADR has the required evidence and cross-target/audit analysis.
- No source, manifest, lockfile, config, help, catalog, or ratchet changes are
  present in the chunk diff.
- Commit with the exact subject:

  `docs(the-61): record dependency adoption ADRs`
