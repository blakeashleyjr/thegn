# Navigation

## ADDED Requirements

### Requirement: Workspaces and worktrees are openable via a frecency-ranked palette mode

superzej SHALL provide a palette mode that lists workspaces and their worktrees
ranked by a frecency score (a pure function of open count and recency, so a
frequently and recently used entry outranks a stale one), filtered by the
existing fuzzy matcher, and selecting an entry MUST switch to that worktree's tab
(the existing one-session tab switch) and update its frecency record. An empty
frecency history MUST fall back to recency order without error.

#### Scenario: Frequently used worktree ranks first

- **WHEN** two worktrees have equal open counts but one was opened more recently
- **THEN** the more recently opened worktree ranks higher in the palette

#### Scenario: Selecting an entry switches tabs and records the open

- **WHEN** the user selects a worktree from the frecency palette
- **THEN** superzej switches to that worktree's tab and bumps its frecency record

### Requirement: A pane's cwd can be resolved to its worktree tab (connect to root)

superzej SHALL provide a "connect to root" action that resolves the focused
pane's current working directory to the owning worktree root via git and switches
to that worktree's tab; when the cwd is inside a registered workspace it MUST
focus the matching tab, and when it is outside any registered workspace it MUST
offer to add it rather than failing silently.

#### Scenario: Nested cwd jumps to its worktree tab

- **WHEN** the focused pane's cwd is a nested subdirectory of a registered
  worktree and the user invokes connect-to-root
- **THEN** superzej switches focus to that worktree's tab

#### Scenario: Cwd outside any workspace offers to add it

- **WHEN** the focused pane's cwd is not under any registered workspace and the
  user invokes connect-to-root
- **THEN** superzej offers to add it as a workspace instead of doing nothing

### Requirement: A repository can be cloned and opened in one action

superzej SHALL provide a clone-and-open action that clones a repository URL off
the event loop, registers it as a workspace, and opens its first worktree tab,
raising a clear error on clone failure without blocking the loop.

#### Scenario: Clone and open lands in a worktree tab

- **WHEN** the user runs clone-and-open with a valid repository URL
- **THEN** the repository is cloned off-loop, registered as a workspace, and its
  first worktree tab is opened

### Requirement: tmuxinator/sesh layouts can be imported as a layout source

superzej SHALL parse a tmuxinator or sesh project file into a neutral layout
description (name, root, and windows with cwd and command) offered as a
worktree/layout source, and a malformed project file MUST produce an error rather
than a panic. The import is read-only and MUST NOT modify the source file.

#### Scenario: A tmuxinator project imports as a layout

- **WHEN** the user imports a valid tmuxinator project file
- **THEN** its windows (name, cwd, command) are available as a layout source for a
  worktree

#### Scenario: A malformed project file is rejected

- **WHEN** the user imports a malformed project file
- **THEN** the import fails with an error and no layout is offered
