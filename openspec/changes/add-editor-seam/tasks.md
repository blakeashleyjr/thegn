## 1. Core seam

- [x] 1.1 `thegn_core::editor`: `Editor` trait (`Probe` + `id`/`caps`/`open`), `OpenRequest`, `EditorLaunch`, `EditorCaps`, `EditorError` (classifying `SeamError`), `JumpSyntax`, `program_profile`, `launch_line`, `is_gui_editor`
- [x] 1.2 Ladder: `TemplateEditor` / `ProgramEditor`, `editor_for` / `editor_with_env` (env injected for tests)
- [x] 1.3 Unit tests: profiles, launch lines per syntax + quoting, ladder order, `open_in` overrides, error classification

## 2. Config

- [x] 2.1 `config_enum! EditorOpenIn` (auto/pane|terminal/external|detached|gui), `EditorConfig`, `Config.editor`; key pin 69 → 70
- [x] 2.2 `[editor]` section in `config/config.toml.example`
- [x] 2.3 `THEGN_EDITOR_COMMAND` / `THEGN_EDITOR_OPEN_IN` in `env_overlay` + `ConfigOverlay` fields; coverage assertions in `env_overlay_covers_every_knob` and `config_overlay_apply_sets_every_field`

## 3. Host migration

- [x] 3.1 `panel_util::editor_launch` / `editor_open_command` backed by the seam; `open_editor` placement-aware helper; `spawn_editor_detached`
- [x] 3.2 Migrate every run.rs editor site (GitAfter::OpenEditor, tests/problems/symbols jumps, Files/Changes o/O/Ctrl-O), actions.rs DrawerCmd::Editor, handlers/panel_changes.rs
- [x] 3.3 `thegn config edit` via the seam; delete `util::editor()`

## 4. Doctor + docs

- [x] 4.1 `editor_probes` in `thegn_svc::seam::registry::probes`; registry test lists `editor`
- [x] 4.2 `docs/ARCHITECTURE.md` seam-table row (and un-stale the git row); `docs/help/configuration.md` `[editor]` highlight
- [x] 4.3 Validation: clippy + targeted suites + `just lint` (pre-existing ratchets reseeded as needed); openspec validate
