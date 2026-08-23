## ADDED Requirements

### Requirement: Editor resolution ladder

The system SHALL resolve the user's editor through `thegn_core::editor::editor_for`, trying in order: a non-empty `[editor] command` template; a concrete `[[tools]] editor` command (with a legacy trailing ` .` stripped and `${…}`-style env-default commands skipped); `$VISUAL`; `$EDITOR`; and finally `vi`.

#### Scenario: Template wins over everything

- WHEN `[editor] command = "myed --file {path} --at {line}"` is set and `$EDITOR` is also set
- THEN `editor_for(cfg).open(req)` renders the template with the request's path (shell-quoted), line and column, and reports id `template`

#### Scenario: Unconfigured user falls through to the environment

- WHEN `[editor] command` is empty and `[[tools]] editor` is the legacy `${EDITOR:-vi} .` default
- THEN the ladder skips both layers and uses `$VISUAL`, then `$EDITOR`, then `vi`

### Requirement: Program-aware launch lines

The system SHALL compose the launch line using the target program's own line-jump syntax from one program table (`program_profile`): `+N` for the vi/emacs family, `file:N[:M]` for helix/zed/sublime/kakoune, `-g file:N[:M]` for the VS Code family, `--line N` for kate/gedit/JetBrains launchers, and no jump for unknown programs. Paths SHALL be shell-quoted via `sh_quote`.

#### Scenario: Line jump follows the program

- WHEN the resolved program is `hx` and the request has line 3
- THEN the command is `hx <path>:3` (quoted only when the target needs it), and for `vim` it is `vim +3 <path>`

### Requirement: Placement per program, overridable

`Editor::open` SHALL return a placement — `Pane` for terminal editors, `External` for windowed editors per the program table when `[editor] open_in = "auto"` — and `open_in = "pane"` or `"external"` SHALL force the placement. The host SHALL honour it: `Pane` opens a center tab (or split), `External` spawns the command detached and reaped, leaving no dead pane.

#### Scenario: Windowed editor detaches

- WHEN `$EDITOR` resolves to `code` with `open_in = "auto"` and a panel "open in editor" action fires
- THEN the launch is spawned via `spawn_detached_reaped` and no center tab is created

### Requirement: Editor config and env knobs

The config SHALL expose `[editor] command` and `[editor] open_in` (`auto`/`pane`/`external`, with `terminal` and `detached`/`gui` accepted as aliases), overridable via `THEGN_EDITOR_COMMAND` and `THEGN_EDITOR_OPEN_IN`.

#### Scenario: Env overlay

- WHEN `THEGN_EDITOR_COMMAND=hx {path}` and `THEGN_EDITOR_OPEN_IN=external` are set
- THEN the loaded config's `editor.command` and `editor.open_in` reflect them

### Requirement: One host chokepoint

Every host "open in editor" path (panel keys, drawer command, git after-actions, test/problem/symbol jumps, `thegn config edit`) SHALL obtain its command through the editor seam (`panel_util::open_editor` / `editor_open_command` in the compositor; `editor_for` in the CLI). `util::editor()` SHALL NOT exist.

#### Scenario: CLI config edit uses the ladder

- WHEN `thegn config edit` runs with `[editor] command` set
- THEN the template — not `$EDITOR` — opens the config file
