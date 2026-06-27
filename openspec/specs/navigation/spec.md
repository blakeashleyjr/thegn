# Navigation

## Purpose

superzej is navigated entirely from the keyboard across a small set of focusable
chrome zones (sidebar, center panes, right panel, bottom drawer). Focus moves
predictably between zones and panes, and the user can lock focus to the center so
movement keys reach the running program instead of the chrome.

## Requirements

### Requirement: Focus moves between focusable chrome zones

Focus SHALL be movable between the focusable zones — sidebar, center panes, right panel, and bottom drawer — and the currently focused zone MUST be visually indicated.

#### Scenario: Move focus across zones

- **WHEN** the user issues a directional focus-move from the sidebar toward the
  center
- **THEN** focus moves to the center panes and the focused zone is visually
  indicated

#### Scenario: Focus the sidebar directly

- **WHEN** the user invokes the sidebar focus action
- **THEN** the sidebar becomes the focused zone for keyboard navigation

### Requirement: Focus lock pins input to the center panes

A focus-lock action SHALL pin keyboard focus to the center panes so that movement keys are delivered to the running program rather than cycling chrome zones, and toggling it off MUST restore zone navigation.

#### Scenario: Locked focus reaches the pane

- **WHEN** focus lock is enabled and the user presses a movement key
- **THEN** the key is delivered to the focused center pane, not consumed as a
  zone switch

#### Scenario: Unlock restores zone navigation

- **WHEN** focus lock is toggled off
- **THEN** directional movement again cycles between chrome zones

### Requirement: Alt cycles tabs and worktrees

Alt+Left/Right SHALL cycle tabs within the active worktree and Alt+Up/Down SHALL move between worktrees, with each worktree restoring its own active tab — distinct from the directional focus-zone movement.

#### Scenario: Cycle tabs in the active worktree

- **WHEN** the user presses Alt+Right
- **THEN** the next tab within the active worktree is shown

#### Scenario: Move between worktrees

- **WHEN** the user presses Alt+Down
- **THEN** the next worktree is activated, restoring that worktree's own active
  tab
