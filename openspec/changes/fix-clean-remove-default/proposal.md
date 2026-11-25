# Fix clean worktree removal default

## Why

Removing a clean worktree (Alt-X, or `d` with confirmation on) opened a menu
whose first row, "delete from disk", was pre-selected, so a reflexive Enter
deleted the checkout's files. The menu never said the branch and its commits
survive, so a user could not tell the removal was recoverable from the menu.

## What Changes

- The clean-tree remove menu pre-selects the non-destructive "keep files
  (safe)" arm instead of "delete from disk".
- The delete row reads "delete worktree files from disk (branch kept)", the
  wording the sidebar chooser already uses.
- The menu body states that the branch and its commits are kept, and names
  the worktree(s).
- The dirty-tree menu is unchanged: it already pre-selects Cancel.

## Impact

Affected specs: `sidebar` (clean-removal requirement added).

Affected code: `thegn-host` `menu::delete_worktree_menu` (default index,
labels, body) and its unit tests. The destroy path already passes
`delete_branch = false` on this route, so no git behaviour changes.

Help: `docs/help/quick-reference.md` Alt-X row.
