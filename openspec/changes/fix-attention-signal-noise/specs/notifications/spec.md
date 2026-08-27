# notifications

## ADDED Requirements

### Requirement: Clearing the inbox clears exactly what the inbox displays

The repo-scoped inbox's "clear all" SHALL mark read exactly the set the
repo-scoped inbox displays, evaluated by one shared predicate — untagged
(host-global) rows, rows tagged with one of the repo's registered worktrees, and
rows tagged with a worktree path the registry does not know (the repo's main
checkout, an externally-created worktree). It MUST NOT mark read a row tagged
with a known worktree of a different repo. The all-worktrees view SHALL clear
every row regardless of tag. Clearing MUST also lower the live raised hands for
the same scope, so a quieted worktree does not have its demand returned by the
next hydration.

#### Scenario: A row tagged to the repo's main checkout is displayed and cleared

- **WHEN** a notification is tagged with the repo's own main checkout path,
  which never gets a worktree-registry row, and the user clears all in the
  repo-scoped inbox
- **THEN** the row is both displayed by the inbox and marked read, and it stays
  read across a rehydrate

#### Scenario: Another repo's known worktree is neither displayed nor cleared

- **WHEN** a notification is tagged with a registered worktree belonging to a
  different repo and the user clears all in the repo-scoped inbox
- **THEN** the row is not displayed and is left unread

#### Scenario: An untagged row is displayed and cleared

- **WHEN** a host-global notification carrying no worktree tag is in the inbox
  and the user clears all in the repo-scoped inbox
- **THEN** the row is displayed and marked read

#### Scenario: The all-worktrees view clears everything

- **WHEN** the inbox has been widened to every worktree and the user clears all
- **THEN** every unread notification is marked read regardless of its worktree
  tag, and every live raised hand is lowered

#### Scenario: Clearing lowers the live raised hands

- **WHEN** a worktree with a live raised hand is acknowledged, or the inbox's
  clear-all runs over a scope containing it
- **THEN** the live per-session attention state for that worktree is deleted and
  the worktree does not return to the needs-you state on the next hydration
