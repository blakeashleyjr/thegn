# File Explorer

## Purpose

superzej provides per-worktree file browsing, preview, and search through a yazi
drawer carved into the chrome as a reserved, focusable panel, plus project-wide
fuzzy find and ripgrep search. File-management actions run from the tree, and the
external file tools are memory-capped so a runaway preview process cannot take
down the terminal.

## Requirements

### Requirement: Yazi drawer is a reserved focusable panel

The file drawer SHALL be a reserved chrome region (a focusable `Drawer` zone) toggled by a keybind, sized as part of layout computation, with its open/closed state persisted per worktree.

#### Scenario: Toggle the drawer

- **WHEN** the user toggles the file drawer
- **THEN** a reserved drawer region opens, is focusable, and its state is
  remembered for that worktree

### Requirement: Project search and fuzzy file find

The explorer SHALL provide fuzzy file finding and ripgrep project search scoped to the workspace.

#### Scenario: Ripgrep project search

- **WHEN** the user runs a project content search
- **THEN** ripgrep results across the worktree are returned

### Requirement: File management from the tree

The drawer SHALL support new/rename/delete (delete with confirmation) and show file-type icons and git/VCS-status colors.

#### Scenario: Delete asks first

- **WHEN** the user deletes a file from the tree
- **THEN** a confirmation is required before the file is removed

### Requirement: File tools are memory-capped

External file/preview tool processes launched by the drawer SHALL run under a memory cap so a runaway tool cannot OOM the terminal.

#### Scenario: Runaway preview is contained

- **WHEN** a launched file tool exceeds its memory cap
- **THEN** it is constrained by the cap rather than exhausting host memory
