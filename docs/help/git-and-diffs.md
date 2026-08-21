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

`Alt-/` opens a plain `git diff` in a pane; `thegn wt diff` prints one
from any shell.

## Syncing

Push, pull, and fetch act on the **focused worktree**. They have no
default chords — run them from the [[command-palette]], or bind them by
id in `[keybinds]`:

| Action     | Id          |
| ---------- | ----------- |
| Git: push  | `git-push`  |
| Git: pull  | `git-pull`  |
| Git: fetch | `git-fetch` |

Because each worktree is its own checkout, these never touch a branch you
are not looking at — which is what makes running several at once safe.

## Rollback

`rollback` restores a worktree to a prior snapshot when a run goes wrong
— the undo for "the agent made a mess". It is palette-runnable and
bindable like the sync actions.

## Which git does what

thegn reads git natively where it can and falls back to the `git` CLI
otherwise, so nothing depends on a feature the native backend has not
covered yet. Writes always go through the CLI.

One consequence worth knowing: **git is the source of truth**, and
thegn's database is a cache. Deleting a worktree with `git worktree
remove` behind thegn's back is safe — the tree reconciles on the next
launch.

## Landing work

Diffs are where a branch starts; [[merge-queue]] is where it ends. The
fold-actor merges queued branches entirely in the git object database
without checking anything out, so it works even while you are using the
repo. See also [[review-a-pr]].
