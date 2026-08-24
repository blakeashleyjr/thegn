# editor Specification (delta)

## ADDED Requirements

### Requirement: Project-level launch lines

The program table SHALL know how to open a **directory as a project** for the
programs it profiles: the VS Code family, JetBrains launchers, zed, and
sublime open the root as their project argument; terminal editors resolve to
`Pane` placement with the working directory at the root. A configured
`[editor] command` template SHALL be rendered for project opens with the root
as `{path}` and no line/column, so template users need no new keys. Paths
SHALL be shell-quoted via the existing quoting path.

#### Scenario: VS Code opens the worktree as a project

- **WHEN** the resolved editor is `code` and a project open is requested for
  a worktree root
- **THEN** the launch line is `code <quoted-root>` with no `-g` jump, and the
  placement is `External`

#### Scenario: Terminal editor opens a pane at the root

- **WHEN** the resolved editor is `hx` and a project open is requested
- **THEN** the placement is `Pane` with the pane's working directory at the
  worktree root

### Requirement: Worktree handoff goes through the editor seam

The system SHALL provide an `open-in-ide` action — in the command palette and
on the sidebar worktree-row menu — that opens the targeted worktree's root as
a project through the editor seam, honoring the resolution ladder (template →
per-workspace `[[tools]] editor` override → `$VISUAL`/`$EDITOR` → `vi`) and
the placement rules (`External` spawns detached and reaped; `Pane` opens a
center tab). The action MUST NOT bypass the seam with its own editor
resolution.

#### Scenario: Row-menu handoff on a non-active worktree

- **WHEN** the user invokes `open-in-ide` from another worktree's sidebar row
  while `$EDITOR` resolves to a GUI editor
- **THEN** that row's worktree root opens in the GUI editor via a detached,
  reaped spawn, and no center tab is created

#### Scenario: Per-workspace override wins

- **WHEN** a workspace's `[[tools]] editor` override names a different editor
  than the global config
- **THEN** `open-in-ide` in that workspace launches the override editor
