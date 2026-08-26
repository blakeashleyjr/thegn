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

- **changes** — the working diff. `↵` on a file inlines its hunks
  (binary and mode-only changes say so instead of a diff); `space`
  stages; `e` widens to the full-screen lazygit-style git frame, where
  `↵` drills into **staging** (line-level stage/unstage).
- **commits** — the branch's log; pick a commit to view or operate on.
  `E` edits (interactive rebase stopping at the commit) — bare `e` is
  always the panel width cycle.
- **branches** — local branches: check out, create, delete; open-PR
  badges ride each row. In the wide frame the main region shows the
  **selected** branch's own recent commits (HEAD's while it loads).
- **stash** — stash list: apply, pop, drop; `↵` shows the stash's real
  diff (`git stash show -p -u`, untracked files included) in the main
  region.
- **files** — the worktree's file tree; `/` filters it (directories stay
  while any descendant matches; Esc clears), `↵` previews a file inline,
  `o` pages it in bat, `O` opens your editor, `y` reveals it in yazi.

Inside the git-family sections (changes/commits/branches/stash — not
files), `?` shows that section's own key cheatsheet. Marks, ranges, and
flows follow lazygit conventions. An action key that needs a wider view
**widens the panel and performs the action** in one press (the status
crumb says so); drill detail views at the half width carry a breadcrumb
(`changes › staging · esc back`) so the way back is always visible.

`Alt-/` opens a plain `git diff` in a pane; `thegn wt diff` prints one
from any shell.

## Structural diffs (difftastic)

`[git] structural_diff` renders the **read-only** diff surfaces — the
full-screen diff view and `thegn diff --structural` — through
[difftastic](https://difftastic.wilfred.me.uk/) instead of a line diff:

- `off` (default) — thegn's internal unified view.
- `auto` — structural when `difft` resolves (config override → `PATH` →
  a managed download), the internal view otherwise.
- `difft` — always structural; on any failure (tool missing, timeout,
  oversized change) it falls back to the internal view with a one-line
  notice, never blocking.

In the full-screen structural view, **`t`** toggles between the
structural render and the internal unified view. Setting `structural_diff`
also makes `Alt-/` open this native view even when a `[[tools]] diff` is
seeded — an explicit opt-in wins over the default.

Structural output is **read-only**: it is never fed to `git apply`, and
every _stageable_ diff (inline hunks, line staging) keeps the sanitized
flags regardless of this setting. Acquire/inspect `difft` via
`[managed_tools.difft]` and `thegn doctor` (its **Source-control
workflow posture** section also reports the git version against the
merge-queue fold's floor, jj colocation, and any custom merge drivers).

## Coexisting with jujutsu

In a repo colocated with [jujutsu](https://jj-vcs.github.io/) (a `.jj/`
beside `.git/`), thegn stays out of the way rather than fighting it:
worktree rows carry a jj marker, detached `HEAD` (jj's normal state) is
not shown as an error, staging surfaces note that jj ignores the git
index, and background `auto_fetch` skips the repo unless
`[git] auto_fetch_colocated = true`. Reads are unchanged; mutations are
warned about, not blocked.

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
