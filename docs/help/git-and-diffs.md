---
id: git-and-diffs
title: Git & diffs
order: 10
contexts:
  [panel:changes, panel:commits, panel:branches, panel:stash, panel:files]
actions: [git-push, git-pull, git-fetch, rollback]
---

# Git & diffs

The [[panel]]'s **git** tab is a lazygit-style set of sections for the
focused worktree; `Alt-g` opens the real lazygit in a pane when you want
the full tool.

- **changes** — the working diff. `↵` on a file inlines its hunks; `e`
  cycles to a full-screen side-by-side view; staging keys mirror lazygit.
- **commits** — the branch's log; pick a commit to view or operate on.
- **branches** — local branches: check out, create, delete.
- **stash** — stash list: apply, pop, drop.
- **files** — the worktree's file tree; `↵` previews a file inline.

Inside any git-family section, `?` shows that section's own key
cheatsheet. Marks, ranges, and flows follow lazygit conventions.

`Alt-/` opens a plain `git diff` in a pane; `thegn diff` prints one from
any shell.

Push, pull, and fetch for the focused worktree run from the palette (or
your own `[keybinds]`); **rollback** restores a worktree to a prior
snapshot when a run goes wrong.
