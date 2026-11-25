## 1. Clean-removal menu

- [x] 1.1 Pre-select the "keep files (safe)" row in `menu::delete_worktree_menu`.
- [x] 1.2 Relabel the delete row "delete worktree files from disk (branch kept)".
- [x] 1.3 Set the menu body to name the kept branch and its commits, with the worktree names.
- [x] 1.4 Unit-test the default row, the keep-files pick on Enter, the branch-kept label and body.

## 2. Help

- [x] 2.1 Update the Alt-X row in `docs/help/quick-reference.md`.

## 3. Verification

- [ ] 3.1 Run the focused `menu` nextest filters and `just ci` (includes openspec-validate).
- [ ] 3.2 Check the Alt-X menu live: the highlighted row is "keep files (safe)" and the body names the kept branch.
