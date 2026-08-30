# Chunk 1 — core editor seam, target policy, and configuration

Commit subject (exact): `feat(the-17): extend editor seam for IDE handoff`

## Files touched

- `crates/thegn-core/src/editor.rs`
- `crates/thegn-core/src/editor/providers.rs` (new)
- `crates/thegn-core/src/editor/vscode.rs` (new)
- `crates/thegn-core/src/editor/cursor.rs` (new)
- `crates/thegn-core/src/editor/zed.rs` (new)
- `crates/thegn-core/src/editor/jetbrains.rs` (new)
- `crates/thegn-core/src/editor/nvim_remote.rs` (new)
- `crates/thegn-core/src/editor/emacs.rs` (new)
- `crates/thegn-core/src/config.rs`
- `config/config.toml.example`
- `docs/help/configuration.md`

## Approach

Extend the existing synchronous `thegn_core::editor::Editor` seam; do not
create an IDE trait. Replace shell-only launch planning with a structured,
substrate-free argv plan while retaining the existing custom template/program
ladder for compatibility. Add a validated `EditorTarget` for a worktree root,
optional worktree-relative file, line, and column. Normalize and reject
escape paths in pure code; make directory/project open an explicit optional
operation with `Unsupported` for providers that cannot do it.

Register one provider implementation per logical kind: `vscode`, `cursor`,
`zed`, `jetbrains`, `nvim_remote`, and `emacs`. Keep every executable spelling
and vendor-specific argv shape inside its implementation file. Provider caps
must exactly match optional operations; no provider may claim column/project
support it cannot implement. `auto` retains the existing custom-program
ladder and never performs a PATH scan during a UI action. The provider
registry supplies cheap `Probe` reports for `thegn doctor`.

Add `[editor].provider` and `[workspace.<slug>].editor` as trusted config. The
workspace override selects a logical provider and inherits when absent; it is
not part of repo-local `.thegn.*`. Add `THEGN_EDITOR_PROVIDER` through the
existing env-overlay mechanism and tests. Document all keys in the example and
configuration help. Preserve `[editor].command` and `[editor].open_in`
semantics, with a non-empty explicit command remaining the highest-priority
custom launch layer.

Add focused unit tests for target validation, every provider’s argv/placement/
caps, unsupported operations, provider precedence, environment overlay, and
custom-program compatibility. Do not call `which`, spawn, or touch tokio from
core tests or implementations.

## Dependencies and overlap

This chunk is independent of chunks 2 and 3 at the file level. It must land
first logically: chunk 2 consumes the core handoff request for the control
wire, and chunk 3 consumes the target/launch types. No other coder should
touch the listed core/config files concurrently. THE-27 is not a file overlap
but chunk 3 is serial after THE-27 lands and rebases onto this chunk.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core editor`
- `cargo nextest run -p thegn-core config`
- `cargo nextest run -p thegn-core env_overlay`

Also run the scoped config-example/schema test if its package filter is
available in this checkout; do not substitute `just test` or a workspace
build. Keep `test/completion-slot-ratchet.txt` unchanged unless a test exposes
an actual new CLI value slot.

## Done criteria

- The existing `Editor` seam is the only editor/IDE abstraction.
- Core contains no tokio, termwiz, subprocess, filesystem probing, or vendor
  CLI references outside provider implementation modules.
- Targets are worktree-contained and pure; file, directory, line, and column
  behavior is unit-tested, including unsupported/degraded cases.
- `thegn doctor` can enumerate provider probes without changing the TUI loop.
- Global and per-workspace provider config, env precedence, example docs, and
  help docs are complete; env/help ratchets have no new excuse.
- The coder commits exactly as:
  `feat(the-17): extend editor seam for IDE handoff`
