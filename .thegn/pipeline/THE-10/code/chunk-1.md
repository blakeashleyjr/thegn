# Chunk 1 — core config compatibility and catalog aliases

Commit subject (exact): `fix(the-10): add project config compatibility and program capability aliases`

## Scope

Make the canonical config and capability contracts project/program-shaped
without breaking old config, environment variables, capability consumers, or
the control schema. This chunk must land before host/UI work.

## Exact files touched

- `crates/thegn-core/src/config_compat.rs` (new pure compatibility module)
- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_ui.rs`
- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-core/src/config_write.rs`
- `crates/thegn-core/src/config_tests.rs`
- `crates/thegn-core/src/config_tests_coverage.rs`
- `crates/thegn-core/src/capability.rs`
- `crates/thegn-core/tests/config_example.rs`
- `crates/thegn-core/tests/env_overlay_coverage.rs` (only if the new alias
  coverage requires a test fixture change)
- `crates/thegn-core/tests/hm_module_drift.rs`
- `nix/hm-module.nix`
- `config/config.toml.example`
- `test/env-overlay-ratchet.txt`
- `test/surface-gaps-ratchet.txt` (only if mirrored alias gaps are required by
  the catalog projection implementation)

Validation-only, not edited unless the generator proves a real wire change:
`crates/thegn-svc/tests/control_schema.rs` and
`docs/api/control-v1.json`.

## Approach

1. Add a substrate-free compatibility normalizer with N = 3 stable releases.
   Recognize `projects_dir`/`workspaces_dir`, `[project.<slug>]`/
   `[workspace.<slug>]`, the two UI keys, and
   `THEGN_PROJECTS_DIR`/`THEGN_WORKSPACES_DIR`. Canonical wins on duplicates;
   return diagnostics naming both exact keys. Keep tolerant load behavior and
   make strict validation accept legacy keys as known deprecated keys.
2. Expose canonical serde/schema/example/HM names. Keep internal fields and
   DB-facing types stable for this chunk. Make canonical `projectsDir` the Nix
   option and retain `workspacesDir` as deprecated input; render only
   `projects_dir`. Preserve tracker-owned workspace/project keys.
3. Add canonical `program.*` catalog rows for the six multi-repo verbs and
   deprecated `project.*` rows with identical verb, summary, since, scope, and
   surfaces. Update the one-row test to one canonical row per verb, make
   `for_verb` canonical, and make lookup/surface/coverage parity explicit.
   Keep existing CLI-only surface gaps mirrored or canonicalized deliberately;
   do not add routes.
4. Add pure unit tests for alias precedence, warnings, validator acceptance,
   env precedence, tracker-key non-renaming, catalog alias parity, and
   control-schema stability. Keep config and catalog logic out of host and
   vendors.

## Overlap and dependency

No file overlaps Chunk 2 or Chunk 3. Chunk 2 depends on the canonical config
and catalog identifiers from this chunk and therefore runs after it. Chunk 3
depends on the generated names from both earlier chunks and runs after Chunk 2.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core config --no-fail-fast`
- `cargo nextest run -p thegn-core capability --no-fail-fast`
- `cargo nextest run -p thegn-core hm_module_drift --no-fail-fast`
- `cargo nextest run -p thegn-core env_overlay --no-fail-fast`
- `cargo nextest run -p thegn-svc control_schema --no-fail-fast`

Use a temporary `XDG_STATE_HOME` for any executable invocation. Do not run a
migration, use the live state DB, run e2e, or start a full-workspace build.

## Done criteria

- Canonical config schema/example/HM output uses project spellings; every old
  spelling loads for exactly the documented N-release window and emits a
  warning naming the key and replacement.
- Canonical value wins deterministically when both spellings are present, and
  `config validate` reports the duplicate without an unknown-key false
  positive.
- `THEGN_PROJECTS_DIR` is exercised; `THEGN_WORKSPACES_DIR` remains accepted
  and warned; env-overlay coverage and ratchet are green.
- Six `program.*` rows and six deprecated `project.*` aliases are present;
  aliases project identically and catalog tests prove it. No DB or control
  route changes are made.
- Scoped tests above pass, and the coder commits exactly:
  `fix(the-10): add project config compatibility and program capability aliases`.
