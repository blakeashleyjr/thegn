---
id: review-a-pr
title: Review a PR
parent: workflows
order: 1
---

# Review a PR

1. The masthead's CI/notification cluster (see [[bars]]) surfaces PRs that
   need you; `↵` on the item shows the detail popup.
2. Check out the branch as its own worktree: `Alt-w` in the repo's
   workspace, or the palette's `@` git mode to pick the branch. The
   worktree is isolated — your own work stays untouched.
3. The right [[panel]]'s **work** tab tracks the branch's PR: state, CI
   check rollup, review decision. The **git** tab's _changes_ section is
   the diff; `e` widens it to full-screen, side by side.
4. Comment/approve via `gh` in the pane, or open the PR view from the
   panel for the conversation feed.
5. Done? `Alt-X` removes the review worktree; the branch stays.

> Tip: `thegn pr` runs the same PR summary non-interactively from any
> shell.
