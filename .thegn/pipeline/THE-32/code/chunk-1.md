# Chunk 1 — core submodule domain and config contract

Commit subject (exact): `feat(the-32): add core submodule models and config`

## Files touched

- `crates/thegn-core/src/submodule.rs` (new)
- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/patch.rs`
- `crates/thegn-core/src/forge/model.rs`
- `crates/thegn-core/src/fold.rs`
- `crates/thegn-core/src/agent_task.rs`
- `crates/thegn-core/src/config_git.rs` (new)
- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_ui.rs`
- `crates/thegn-core/src/termcaps.rs`
- `config/config.toml.example`
- `test/env-overlay-ratchet.txt`

No other chunk edits these files. Chunk 2 consumes the public core types;
chunk 3 consumes the diff-row and conflict contracts. This is a serial API
dependency, not a file overlap: land this commit first.

## Approach

1. Add the pure `thegn_core::submodule` model and fixture-string parsers for
   `.gitmodules`, recursive status evidence, gitlink old/new SHAs, direction,
   bounded summaries, path boundaries, and typed pointer conflicts. Reject
   unsafe/ambiguous paths. Include unit tests for clean, moved, rewind,
   diverged, dirty, untracked, uninitialized, malformed, nested, and escaped
   fixtures.
2. Add `FileKind::Submodule` and atomic-selection validation to `patch.rs`.
   Recognize mode `160000` and `Subproject commit`; round-trip add/move/delete
   fixture patches. Extend forge unified-diff parsing with the same marker and
   fixture tests. Extend fold/prompt data only with typed conflict details;
   preserve raw git paths for operations.
3. Add `SubmoduleMode::{Auto,Off}` through the exhaustive config layering and
   env overlay (`THEGN_GIT_SUBMODULES`), plus
   `[ui] sidebar_show_submodules`. Defaults are `Auto` and `true`; repo-local
   untrusted overlays cannot change the lifecycle mode. Add the width-1
   Unicode/ASCII submodule glyph to the core capability table and its existing
   width/degradation tests.
4. Keep this commit substrate-free. There must be no `Command`, tokio,
   termwiz, filesystem walk, network call, or vendor SDK in the new core code.

## Tests to run

Use hermetic fixture setup only; local submodule fixtures may pass
`-c protocol.file.allow=always` and `-c commit.gpgsign=false`, and any DB test
must set `XDG_STATE_HOME` to a fresh temp directory.

- `just quick thegn-core`
- `cargo nextest run -p thegn-core submodule`
- `cargo nextest run -p thegn-core patch`
- `cargo nextest run -p thegn-core fold`
- `cargo nextest run -p thegn-core config`

Do not run a full workspace gate or the built binary.

## Done criteria

- All listed core tests pass and new pure branches have unit coverage suitable
  for the core 95% gate.
- Gitlink patches and forge diffs are classified as atomic submodules; no
  partial selection can reach a line-stage operation.
- Old/new SHA direction and pointer-conflict formatting are deterministic over
  fixture strings/facts.
- `config/config.toml.example` documents both new keys in the same commit as
  their schema fields, the env overlay is covered by its ratchet, and the
  existing control/completion snapshots remain unchanged. Help prose is owned
  by chunk 3.
- `git diff --check` is clean.
- Commit exactly as `feat(the-32): add core submodule models and config`.
