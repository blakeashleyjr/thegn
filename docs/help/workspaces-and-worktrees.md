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
  inside a sandbox. Configured `pre_create` hooks run before git creates it;
  `post_create` hooks run after registration and existing provisioning.
- Saved `[[worktree_templates]]` presets (and existing tmuxinator/sesh
  project files) appear in the "what to run" picker and the
  [[command-palette]].

Lifecycle hooks apply to every creation door, including the wizard, `wt new`,
issue dispatch, and the daemon's `worktrees.create` request. A blocking
`pre_create` leaves both git and the database unchanged. The wizard schedules
the default asynchronous `post_create` work before its completion event; a
`wait = true` entry gates the first pane. Headless CLI creation waits for its
post-create job before returning, while daemon creation reports the existing
worktree response and completes post-create asynchronously.

## Lifecycle and sessions

`pre_destroy` runs before provider teardown and git removal, and `post_destroy`
runs from the repository root after a successful removal. User deletion keeps
the worktree visible when a blocking pre-destroy hook fails; the existing
delete confirmation supplies the force/delete-anyway path. Explicit workspace
cleanup and wizard rollback use force semantics so speculative worktrees do
not leak. Automatic merge reclaim is unattended: it reports hook failures and
continues only after the existing clean guard.

`session_start` runs once when the first pane for a worktree session is about
to spawn, and `session_end` runs once when the last pane exits or its tab
closes. Both are asynchronous and never delay pane creation or tab close. See
[[configuration]] for the event lists, working directories, environment, and
repo trust behavior.

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
