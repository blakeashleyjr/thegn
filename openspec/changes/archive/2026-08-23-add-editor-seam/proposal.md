## Why

"Open in editor" was assembled ad hoc in the host: `panel_util::editor_open_command` read `[[tools]] editor`, hard-coded the `+{line}` vi syntax for every program, and each call site decided tab-vs-detached itself (one Ctrl-O site spawned detached, the rest always opened a tab — so `code` opened a dead pane that exited immediately). The CLI (`thegn config edit`) had a third, env-only resolution path. The audit's row A4 calls for the editor to be a first-class provider seam like forge/git/ci: one resolution ladder, program-aware line-jump syntax, honest capabilities, a probe in `thegn doctor`, and config + env knobs.

## What Changes

- New `thegn_core::editor` seam: `Editor` trait (`Probe + id/caps/open`), `OpenRequest{path,line,col}`, `EditorLaunch{command,placement}`, `EditorCaps{line,column,external}`, `EditorError` (classifying `SeamError`).
- Resolution ladder: `[editor] command` template (`{path}`/`{line}`/`{col}`) → concrete `[[tools]] editor` (legacy ` .` suffix stripped; `${…}` defaults skipped) → `$VISUAL` → `$EDITOR` → `vi`.
- Program profile table (`program_profile`): jump syntax (`+N`, `file:N[:M]`, `-g file:N`, `--line N`), column support, windowed-vs-terminal — one table shared with `util::is_gui_editor`.
- New config: `[editor] command`, `[editor] open_in = "auto"|"pane"|"external"` (+ `THEGN_EDITOR_COMMAND` / `THEGN_EDITOR_OPEN_IN` env knobs; key-count pin 69 → 70).
- Placement-aware host helper `panel_util::open_editor`: terminal editors open a center tab (or split), windowed editors spawn detached (`spawn_detached_reaped`) — every editor-open key/action in run.rs, actions.rs and handlers/panel_changes.rs goes through it; `thegn config edit` resolves through the same ladder.
- `editor` seam registered in `thegn_svc::seam::registry::probes` → `thegn doctor` Providers.
- `util::editor()` removed (superseded by the ladder).

## Impact

- tasks.md row A4 (editor seam).
- Specs: new `editor` capability; `provider-seams` gains the editor row.
- Code: thegn-core (`editor.rs`, config), thegn-svc (registry), thegn-host (panel_util + call sites, cmd/config).
