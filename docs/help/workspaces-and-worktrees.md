---
id: workspaces-and-worktrees
title: Workspaces & worktrees
order: 2
actions:
  [
    new-worktree,
    new-workspace,
    delete-workspace,
    close,
    close-worktree,
    close-tab,
    switch-workspace,
    next-worktree,
    prev-worktree,
    next-workspace,
    prev-workspace,
    new-worktree-from-template,
  ]
---

# Workspaces & worktrees

The two core objects. A **workspace** is a repo; a **worktree** is a git
worktree inside it, shown as a tab. git is the source of truth — thegn's
database only caches and resurrects what git already knows.

## Creating

- `Alt-W` — **new workspace**: pick a repo from your scanned roots
  (`repo_roots` in the config) or paste a git URL to clone and open.
- `Alt-w` — **new worktree**: branches off the base branch
  (`base_branch = "auto"` follows the current branch), names it with your
  `branch_prefix`, opens a tab, and asks what to run: a coding agent from
  `[[agents]]`, a tool from `[[tools]]`, or a plain shell — optionally
  inside a sandbox.
- Saved `[[worktree_templates]]` presets (and existing tmuxinator/sesh
  project files) appear in the "what to run" picker and the
  [[command-palette]].

## Switching

- `Alt-↑/↓` — previous/next worktree within the workspace.
- `Shift-Alt-↑/↓` — previous/next workspace.
- `Alt-o` — workspace switcher; `Alt-1..9` / `Ctrl-1..9` jump by sidebar
  slot; the palette's `~` mode ranks everything by frecency.

## Closing and deleting

- `Alt-x` closes the focused pane if the tab is split, else the tab.
- `Alt-X` removes the worktree and its tab; **the branch is kept**.
- Deleting from disk is always an explicit second step in the [[sidebar]]
  delete menu — nothing destructive rides on a single keystroke.

Worktrees live under `~/.thegn/worktrees/<repo>/<branch-slug>` by default;
`worktree_mode = "in_repo"` keeps them in `<repo>/.worktrees`. See the
[[config-reference]] for every knob.
