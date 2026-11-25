# Sidebar

## ADDED Requirements

### Requirement: Clean worktree removal is non-destructive by default

The clean-tree remove menu (Alt-X, or the delete key with confirmation
enabled) SHALL pre-select the non-destructive keep-files arm. Its delete arm
SHALL be labelled as keeping the branch, and the menu body SHALL state that the
branch and its commits are kept and name the affected worktrees. No delete
option SHALL delete the branch ref on this path.

#### Scenario: Enter on a clean worktree keeps its files

- **WHEN** the user removes a clean worktree with Alt-X and presses Enter on the
  pre-selected row
- **THEN** the checkout is detached and its files stay on disk, and the branch
  is kept

#### Scenario: The delete row names the kept branch

- **WHEN** the clean remove menu is shown
- **THEN** the delete-from-disk row reads "delete worktree files from disk
  (branch kept)" and the body says the branch and its commits are kept

#### Scenario: Dirty targets keep the Cancel default

- **WHEN** the remove targets a worktree with uncommitted changes
- **THEN** the menu names the dirty worktrees, warns that work will be lost, and
  pre-selects Cancel
