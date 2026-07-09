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

### Requirement: Quick-Open two-pass ranking

Fuzzy file open SHALL rank candidates in two passes so that tracked files appear
in a first pass and gitignored or untracked files surface in a second pass after
them, ensuring a tracked file outranks a gitignored file at an equal fuzzy score
while still keeping gitignored files reachable; this ranking uses the existing
fuzzy matcher and depends on no AI/agent layer.

#### Scenario: Tracked files rank before gitignored at equal score

- **WHEN** a tracked file and a gitignored file have the same fuzzy score for the
  query
- **THEN** the tracked file is listed before the gitignored file

#### Scenario: Gitignored files surface in the second pass

- **WHEN** the query matches only gitignored or untracked files
- **THEN** those files are still listed, appearing in the second pass

#### Scenario: No gitignored matches

- **WHEN** the query matches only tracked files
- **THEN** the results contain just the first-pass tracked files with no empty
  second segment shown
