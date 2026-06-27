# Command Palette

## Purpose

The command palette is the in-process launcher and search surface: a single
keybind opens a fuzzy palette that can run any bound action, launch a program,
jump to any workspace/worktree, open files, and search file contents. It is a
native TUI with prefix-routed modes backed by a fuzzy matcher and embedded
ripgrep.

## Requirements

### Requirement: Fuzzy palette over actions and navigation targets

A keybind SHALL open a fuzzy command palette that searches bound actions, launchable programs, and workspaces/worktrees, and selecting a result MUST run the action, launch the program, or switch to the target.

#### Scenario: Jump to a worktree

- **WHEN** the user opens the palette and selects a worktree result
- **THEN** the session switches to that worktree's tab

#### Scenario: Run a bound action

- **WHEN** the user selects an action result
- **THEN** that action runs

### Requirement: Prefix-routed search modes

The palette SHALL support prefix-routed modes (e.g. All, Files, Content, Git, Symbols); Content/Files searches MUST use the fuzzy matcher and embedded ripgrep rather than shelling out to an external palette process.

#### Scenario: Content search mode

- **WHEN** the user enters the content-search prefix and a query
- **THEN** the palette returns ripgrep matches across the workspace
