# Chunk 3 — CLI, completion, configuration docs, and help

## Files touched

- `crates/thegn-host/src/cmd/theme.rs`
- `crates/thegn-core/src/completion/catalog.rs`
- `crates/thegn-core/src/completion/sources.rs`
- `config/config.toml.example`
- `docs/help/theming.md` (new)
- `docs/help/index.md`
- `docs/extending/theme.md`
- `crates/thegn-host/src/help/pages.rs`

Do not touch the popup/runtime files from chunk 2, core theme/import files from
chunk 1, ratchet allowlists, control-schema snapshot, or e2e snapshots.

## Approach

Replace the interactive fzf/gum implementation in `cmd/theme.rs` with a
deterministic `theme set <name>`, merged `theme list`, and local-file
`theme import <file> [--name]`. Reuse the chunk-2 store and chunk-1 pure
converter; keep CLI I/O in the CLI process, never in the TUI loop. Remove the
dead `theme.name` write and persist the existing `[theme].preset` key. Do not
add export or base16 commands.

Add every new value-taking argument to the single core completion catalog and
provide bounded user-theme candidates through the existing theme source. Do
not grow `test/completion-slot-ratchet.txt`; catalog classification is the
ratchet-preserving path. Add the popup and CLI claims to a registered help
page, link it from the help index, and document the existing theme config key,
local theme directory, Gogh import format, save/apply/revert, and all modal
keys. Update the theme example comment only; there is no new config key.

Run the env-overlay ratchet unchanged and run the control-schema drift test to
prove this local UI/CLI feature adds no remote capability. Help ratchets remain
empty/shrink-only because the new page claims the feature directly.

## Overlap/dependency

This chunk is serial after chunks 1 and 2: it consumes the core importer and
the host store/action names. It is file-disjoint from both. No coder should
edit the ratchet files just to acknowledge a passing test; only a genuinely
required shrink is allowed by repository policy, and this feature should not
require one.

## Tests to run

- `just quick thegn-core`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host theme`
- `cargo nextest run -p thegn-host completion`
- `cargo nextest run -p thegn-host help`
- `cargo nextest run -p thegn-svc control_wire`

Also run the package's env-overlay and completion-slot ratchet filters if they
are named separately. Do not run e2e, `just test`, `just ci`, or a full
workspace compile. Any manual `thegn` command must set `XDG_STATE_HOME` to a
fresh temporary directory and must not touch the live state DB.

## Done criteria

- `theme set` is headless and writes the real `[theme].preset`; `list` sees
  built-ins and valid users; `import` maps a local Gogh YAML/JSON file and
  saves a validated user theme.
- Completion has catalog entries for every new value argument and the
  completion-slot ratchet remains clean.
- Config example, theme extension docs, help index, and registered help page
  document the existing key/path and complete overlay keyboard model; help and
  env-overlay ratchets pass without allowlist debt.
- Control schema/catalog snapshots are unchanged and their drift test passes.
- Scoped tests pass; no e2e snapshot is re-recorded.
- Commit with exactly: `docs(the-7): wire theme CLI completion and help`
