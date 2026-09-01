# Chunk 3 — User-facing documentation and help ratchets

## Scope

Document the final embedded-skills contract after chunks 1 and 2 land. Keep
this chunk documentation-only; do not add behavior or duplicate implementation
tables.

## Exact files touched

- `docs/cli.md`
- `docs/help/skills.md` (new)
- `docs/help/configuration.md`
- `docs/help/pipeline-board.md`
- `crates/thegn-host/src/help/pages.rs`
- `test/help-ratchet.txt` (generated only if the help ratchet changes)
- `test/help-context-ratchet.txt` (generated only if the help-context ratchet changes)
- `test/help-prose-ratchet.txt` (generated only if the help-prose ratchet changes)
- `test/help-panel-prose-ratchet.txt` (generated only if the panel ratchet changes)

Do not edit source modules, config schema, control schema, or completion files
in this chunk. If a ratchet reports an implementation defect, return it to
chunk 2 rather than weakening the allowlist.

## Approach

1. Add the `thegn skills list`, `show <name>`, and `seed [--worktree <path>]`
   grammar to the CLI reference, including offline behavior, configured user
   directories, supported native project layouts, marker/hash conflict rules,
   exclusions, and the fact that `seed` is per-worktree rather than home sync.
2. Add `docs/help/skills.md`, register it in `help::pages::SOURCES`, and keep
   the page valid under the existing markdown/action/context validator. Explain
   how to write a skill: package directory, bounded frontmatter, prose body,
   harness targets, gate/when values, path-safe name, and command-drift test.
   State explicitly that skills are prose and are never executed by thegn.
3. Document `[skills]` in the configuration help and the example-generated
   configuration reference. Update the pipeline-board page so the three legacy
   skills are described as registry entries without changing their gates.
4. Run the help ratchet update/check and commit only legitimate generated
   changes. A page with no bindable action claims should not create a fake
   action allowlist entry. Check that the help source directory and `SOURCES`
   remain exactly equal.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host help`
- `cargo nextest run -p thegn-host help::pages`
- `cargo nextest run -p thegn-host cli_help`

Run the repository help-ratchet update/check command when required, then rerun
the scoped host tests. Do not run `just test`, `just ci`, e2e, migrations, or a
full-workspace compile. Do not invoke the built binary against the live state
DB; any manual invocation must set `XDG_STATE_HOME` to a fresh temporary
directory.

## Dependency and overlap

This chunk is serial after chunk 2 because its command grammar, status names,
and config keys must match the implemented seams. It is file-disjoint from
chunks 1 and 2 apart from the explicitly generated help ratchets. No coder
should modify implementation code to make a documentation test pass.

## Done criteria

- CLI docs and the new help page describe all config keys and the actual
  per-worktree seed semantics, including a concise writing example.
- Every page under `docs/help` is embedded in `pages.rs`; help validation and
  all help ratchets pass with no unexplained debt additions.
- Pipeline-board documentation retains the existing `mq`/`pipeline`/
  `supervise` behavior statement accurately.
- Commit exactly as: `docs(the-20): document embedded skills`
