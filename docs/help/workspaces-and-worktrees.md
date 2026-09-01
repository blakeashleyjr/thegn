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

With `[git] submodules = "auto"` (the default), a clone or newly-added
worktree that has a root `.gitmodules` initializes its submodules recursively
on the existing background creation worker. Provider and remote
materialization follow the same rule. Initialization failure is non-fatal: the
worktree stays registered and usable, and the progress/error notice says that
submodules were not initialized. Set the mode to `"off"` to skip recursive
clone, initialization, and deeper state reads.

Before initialization, thegn normalizes the configured path/URL pairs and uses
the repository-trust approval flow for a distinct submodule request. Denial or
an unavailable approval never enables `protocol.file.allow` and never blocks
the checkout itself. The mode is trusted user/workspace configuration; a repo
cannot turn it on through its own `.thegn.toml`.

## Switching

- `Alt-↑/↓` — move to the pane above/below; at the top/bottom pane, with no
  pane left in that direction, it switches to the previous/next worktree within
  the workspace. It never focuses the top/bottom bars — that is `Ctrl-↑/↓`.
- `Shift-Alt-↑/↓` — previous/next workspace. One ring: the workspaces in
  sidebar order, then the terminal hosts, wrapping across the boundary.
  Workspaces and hosts you have **collapsed** are stepped over — a folded group
  is one you are not working in, so navigation neither stops on it nor expands
  it. Set `[ui] sidebar_nav_skips_collapsed = false` to visit every group.
- `Alt-o` — workspace switcher; `Alt-1..9` / `Ctrl-1..9` jump by sidebar
  slot; the palette's `~` mode ranks everything by frecency. `Ctrl-<digit>`
  needs a terminal that reports modified keys — [[terminal-compatibility]]
  covers what to do when it does not, and `Alt-o` always works.

## Closing and deleting

- `Alt-x` closes the focused pane if the tab is split, else the tab.
- `Alt-X` removes the worktree and its tab; **the branch is kept**.
- Deleting from disk is always an explicit second step in the [[sidebar]]
  delete menu — nothing destructive rides on a single keystroke.

Worktrees live under `~/.thegn/worktrees/<repo>/<branch-slug>` by default;
`worktree_mode = "in_repo"` keeps them in `<repo>/.worktrees`. See the
[[config-reference]] for every knob.
