---
id: workspaces-and-worktrees
title: Projects & worktrees
order: 2
actions:
  [
    new-worktree,
    new-project,
    new-workspace,
    delete-project,
    close,
    close-worktree,
    close-tab,
    switch-project,
    switch-workspace,
    next-worktree,
    prev-worktree,
    next-project,
    prev-project,
    new-worktree-from-template,
  ]
---

# Projects & worktrees

The two core objects. A **project** is a repo; a **worktree** is a git
worktree inside it, shown as a tab. git is the source of truth — thegn's
database only caches and resurrects what git already knows.

## Creating

- `Alt-W` — **new project**: pick a repo from your scanned roots
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

- `Alt-↑/↓` — move to the pane above/below; at the top/bottom pane, with no
  pane left in that direction, it switches to the previous/next worktree within
  the project. It never focuses the top/bottom bars — that is `Ctrl-↑/↓`.
- `Shift-Alt-↑/↓` — previous/next project. One ring: the projects in
  sidebar order, then the terminal hosts, wrapping across the boundary.
  Projects and hosts you have **collapsed** are stepped over — a folded group
  is one you are not working in, so navigation neither stops on it nor expands
  it. Set `[ui] sidebar_nav_skips_collapsed = false` to visit every group.
- `Alt-o` — project switcher; `Alt-1..9` / `Ctrl-1..9` jump by sidebar
  slot; the palette's `~` mode ranks everything by frecency. `Ctrl-<digit>`
  needs a terminal that reports modified keys — [[terminal-compatibility]]
  covers what to do when it does not, and `Alt-o` always works.

The canonical action ids above use `*-project`. Existing bindings using
`new-workspace`, `delete-workspace`, `switch-workspace`, `next-workspace`,
`prev-workspace`, or `summon-workspace` remain accepted as compatibility
aliases; their chords do not change.

## Closing and deleting

- `Alt-x` closes the focused pane if the tab is split, else the tab.
- `Alt-X` removes the worktree and its tab; **the branch is kept**.
- Deleting from disk is always an explicit second step in the [[sidebar]]
  delete menu — nothing destructive rides on a single keystroke.

Worktrees live under `~/.thegn/worktrees/<repo>/<branch-slug>` by default;
`worktree_mode = "in_repo"` keeps them in `<repo>/.worktrees`. See the
[[config-reference]] for every knob.
