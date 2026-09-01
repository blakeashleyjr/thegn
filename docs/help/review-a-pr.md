---
id: review-a-pr
title: Review a PR
parent: workflows
order: 1
contexts: [panel:pr, panel:ci]
actions: [pr-open, pr-create]
---

# Review a PR

1. The status bar's CI/notification cluster (see [[bars]]) surfaces PRs that
   need you; `↵` on the item shows the detail popup.
2. Check out the branch as its own worktree: `Alt-w` in the repo's
   project, or the palette's `@` git mode to pick the branch. The
   worktree is isolated — your own work stays untouched.
3. The right [[panel]]'s **work** tab tracks the branch's PR: state, CI
   check rollup, review decision. The **git** tab's _changes_ section is
   the diff; `e` widens it to full-screen, side by side. The full-screen diff
   defaults to **Worktree** and keeps local/staging semantics. Press `Tab` to
   switch to **PR review** when a matching snapshot is available; this source
   shows the PR head diff, inline threads, outdated feedback, and top-level
   comments. Stale, loading, unsupported, and absent snapshots are labeled
   rather than attached to local lines.
4. Act from the panel's _pr_ section: `A` approve, `c` comment, `M`
   merge, `r` rerun failed checks, `o` open in the browser — or run `gh`
   in the pane, or open the PR view for the conversation feed. The
   palette's **PR — open in browser** action (`pr-open`) does the same
   from anywhere, when the conversation outgrows a pane.
5. Done? `Alt-X` removes the review worktree; the branch stays.

## Review feedback

In the PR view's **Files** and **Conversation** tabs, `n`/`N` selects the next
or previous thread (unresolved first), and `Enter` jumps to an exact
`path:line` anchor. Outdated and top-level items stay in Conversation and
report that they have no diff anchor. `v` toggles the view-local resolved
threads display. `p` passes the selected thread and `P` passes all unresolved
threads to the configured agent. A live agent pane in this worktree receives
a non-submitting bracketed paste with no trailing newline; otherwise an
explicit confirmation is required before the configured headless PR-review
template runs off-loop. If neither exists, the handoff reports why and does
nothing. Pasting never auto-submits, resolves, approves, merges, or closes a PR.

Press **`e`** in the PR view to **Open in IDE**. A selected file or exact
new-side review thread carries its path and line; an outdated/deleted anchor
opens the file without guessing a line and explains the fallback. Top-level
feedback has no file anchor, so it opens the review worktree itself. The PR
view footer keeps this action visible beside the browser and review handoffs.

## Raising one

Going the other way, **PR — create (web)** (`pr-create`) opens the
compare view for the focused worktree's branch. Neither PR action has a
default chord — run them from the [[command-palette]] or bind them by id
in `[keybinds]`.

> Tip: `thegn pr` runs the same PR summary non-interactively from any
> shell — see [[cli]].
