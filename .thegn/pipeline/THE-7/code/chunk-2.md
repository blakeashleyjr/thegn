# Chunk 2 — host store, popup state, and live preview

## Files touched

- `crates/thegn-host/src/theme_store.rs` (new)
- `crates/thegn-host/src/theme_builder.rs` (new)
- `crates/thegn-host/src/handlers/theme_builder.rs` (new)
- `crates/thegn-host/src/handlers/mod.rs`
- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/keymap.rs`
- `crates/thegn-host/src/keymap_specs.rs`

Do not touch core, CLI, help/docs, config examples, ratchet files, or e2e
snapshots in this chunk.

## Approach

Implement `ThemeStore` as the provider seam. A background worker scans and
debounces `$XDG_CONFIG_HOME/thegn/themes`, reads only bounded regular files,
parses through core, skips corrupt entries with a status result, and atomically
writes validated slugs beneath that directory. Import reads and save writes
are worker operations; the event loop only sends requests, drains results, and
marks damage. Pulse the existing terminal waker after every result. Do not
extend the config watcher with nested theme I/O and do not block before the
first frame.

Implement pure `ThemeBuilder` reducer/render state in its own module. It
snapshots the effective palette, exposes built-in plus store-provided user
catalog names, edits only known core roles, sanitizes pasted paths/names, and
keeps pending import/save/error state explicit. Render a centered `LayerSpec`
through `layer::open_layer`; use only `seg::Tok` palette roles and capability
glyphs for the sidebar/tab/statusbar/diff/pane preview. Show the pure core
contrast audit as warning-only. Use `layer::box_rect` for mouse hit-testing.

Add `ThemeBuilderOpen` end-to-end with the existing keymap/action recipe and a
non-conflicting default chord `Ctrl+Alt+Shift+t`; keep `CycleTheme` and make it
use the merged catalog. Dispatch open/edit/cancel/apply/import/save in the
handler, not in `run.rs`. Apply candidate palettes live through
`chrome::set_palette`; restore the snapshot on Esc; on config reload, reapply
the candidate after the existing `new_cfg.palette()` path. Add the popup at
the existing modal paint order and guard every explicit pane fast path. Opening
or closing and every preview mutation must mark chrome/full damage; the boxed
layer supplies the compositor cover fact.

Route bracketed paste to the builder's path/name field, and route mouse events
to the builder before underlying panes. Keep outside-click behavior consistent
with other modal overlays. Save/apply closes only after the store confirms
success; failures remain visible and never lose the candidate.

## Overlap/dependency

This chunk is serial after chunk 1 because it consumes the core contracts. It is
serial before chunk 3 because the CLI/help chunk reuses `ThemeStore` and the
new action/help identifiers. No other chunk may edit these host paths. The
small `run.rs`/`main.rs` edits are wiring only; new logic belongs in the three
new modules.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host theme_builder`
- `cargo nextest run -p thegn-host render_plan`
- `cargo nextest run -p thegn-host keymap`

Run the existing color-literal, glyph, env-overlay, completion-slot, and help
ratchet checks through their scoped host test filters if they are exposed by
the package. Do not record or run e2e snapshots. Do not run a live `thegn`
process; if one is unavoidable for a focused manual check, set
`XDG_STATE_HOME` to a new temporary directory.

## Done criteria

- Popup opens as a real boxed overlay, has live representative-chrome preview,
  keyboard/mouse/paste handling, cancel revert, apply/save/import result
  handling, and a warning-only contrast badge.
- Preview is truecolor at composition and uses existing palette/glyph tokens;
  no draw-site RGB literals or alternate quantization path exists.
- All watcher/read/write work is off-loop and bounded; the event loop remains
  0%-idle compliant and the overlay does not block first frame.
- Full damage is correct for open/edit/close/reload, including explicit fast
  paths, and config reload cannot clobber an open candidate.
- Action/keymap/help identifiers and existing ratchets remain coherent; no
  capability/control-schema row is added.
- Scoped tests pass; no e2e snapshot is re-recorded.
- Commit with exactly: `feat(the-7): add live theme-builder popup`
