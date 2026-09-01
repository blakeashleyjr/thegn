# Chunk 1 — core drawer metadata and config catalog

Commit subject (exact): `feat(core): add drawer metadata to tool catalog`

## Scope

Extend the existing `[[tools]]`/`NamedCommand` catalog. Do not create
`[[drawer.tools]]`. Add `DrawerScope` and the pure registry/cwd policy in a new
core sibling module, re-export the public types from `config`, and keep the
core crate free of host/PTY/filesystem/provider dependencies.

## Files touched

- `crates/thegn-core/src/config_drawer.rs` — new pure registry, IDs, scope/cwd
  policy, validation, and unit tests.
- `crates/thegn-core/src/lib.rs` — register the sibling module.
- `crates/thegn-core/src/config.rs` — re-export `DrawerScope`, add serde-defaulted
  `NamedCommand.drawer_scope` and `NamedCommand.drawer_cwd`, thread defaults and
  config loading/post-processing as needed.
- `crates/thegn-core/src/config_validate.rs` — update the enum-definition count
  and THE-11 ratchet comment from 90 to 91.
- `crates/thegn-core/src/config_tests_coverage.rs` — update config fixture and
  coverage assertions for drawer metadata/defaults.
- `crates/thegn-core/src/account.rs` — initialize the new `NamedCommand` fields
  in test fixtures/constructors.
- `crates/thegn-core/src/agent_task.rs` — initialize the new fields and reject
  drawer-only metadata on agent entries in semantic validation.
- `crates/thegn-core/src/bundle.rs` — initialize the new fields in bundle
  fixtures/constructors.
- `crates/thegn-core/src/completion/sources.rs` — initialize the new fields in
  completion fixtures.
- `crates/thegn-core/src/config_pipeline.rs` — initialize the new fields in
  pipeline fixtures.
- `crates/thegn-core/src/config_presets.rs` — initialize the new fields in
  preset fixtures.
- `crates/thegn-core/src/config_tests.rs` — initialize/parse the new fields in
  config fixtures.
- `crates/thegn-core/src/editor.rs` — initialize the new fields in editor-tool
  fixtures.
- `config/config.toml.example` — document `drawer_scope`/`drawer_cwd` on the
  existing `[[tools]]` entries, including worktree ATAC and global DB examples.
- `test/env-overlay-ratchet.txt` — pin the two structured `tools.*` metadata
  paths with a reason; there is no index-addressable environment overlay.

## Approach and invariants

`drawer_scope = None` is picker-only. The pure registry emits files first and
then eligible tools in config order, with stable IDs `files` and `tool:<name>`.
Duplicate/empty/dangling entries warn and are omitted without disabling the
files occupant. `drawer_cwd` is validated as relative for worktree scope and
absolute/`~` for global scope; resolution receives explicit paths and does no
I/O. Existing `NamedCommand.command` and `NamedCommand.env` are the only
command/env source. Do not add ATAC code, PATH probing, shell invocation, or
state persistence here.

Because `DrawerScope` is a `config_enum!`, update the schema coverage count and
comment deliberately. Preserve warn-and-default normal loading and strict
diagnostics in `config validate`. The new keys are structured list-entry
metadata, so the env-overlay ratchet is the correct explicit non-knob.

## Overlap/dependency

This chunk is the prerequisite for chunks 2 and 3. It owns all core changes;
chunk 2 also has to update host-side `NamedCommand` struct literals after this
chunk lands. No files overlap with chunk 3. Run chunks serially: chunk 2 after
this commit, then chunk 3 after chunk 2.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core drawer`
- `cargo nextest run -p thegn-core config_validate`
- `cargo nextest run -p thegn-core env_overlay`

Do not run `just test`, `just ci`, a full-workspace compile, e2e, a migration,
or the built binary. No state DB invocation is needed.

## Done criteria

- Existing `[[tools]]` remains the only command catalog; no
  `[[drawer.tools]]` schema is introduced.
- Core tests prove ordering, stable IDs, duplicate/dangling degradation,
  scope/cwd policy, serde defaults, and legacy-compatible config loading.
- Schema enum and env-overlay ratchets pass with deliberate THE-11 notes.
- `config/config.toml.example` documents every new key and shows both scopes.
- `git diff --check` passes.
- Commit exactly as: `feat(core): add drawer metadata to tool catalog`.
