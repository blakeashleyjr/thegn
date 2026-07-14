---
id: getting-started
title: Getting started
order: 1
---

# Getting started

thegn is **one session**. Switching repos or worktrees is a tab switch,
never a session change — quitting and relaunching restores your exact
workspace, worktree, and pane position.

## The mental model

- A **workspace** is a repo. Create or open one with `Alt-W`.
- A **worktree** is a git worktree, shown as a tab. `Alt-w` branches one
  off the base branch and asks what to run in it — a coding agent, a tool,
  or a plain shell, optionally sandboxed.
- A **pane** is a terminal split inside a tab: `Alt-p` smart-splits,
  `Alt-n` splits down, `Alt-N` splits right.

## Moving around

- `Ctrl-←/↓/↑/→` (or `Ctrl-h/j/k/l`) moves focus across one spatial map:
  [[sidebar]] ← center panes → [[panel]], masthead above, status bar below.
- `Alt-←/→` cycles tabs within the worktree; `Alt-↑/↓` cycles worktrees
  within the workspace; `Shift-Alt-↑/↓` cycles workspaces.
- `Alt-1..9` jumps to a worktree, `Ctrl-1..9` to a workspace, by sidebar
  order.
- `Ctrl-Space` opens the [[command-palette]] — every action, fuzzy-searched.

## Closing things

`Alt-x` is one smart **close**: the focused pane if the tab is split,
otherwise the tab. `Alt-X` (Shift) escalates to removing the whole worktree
and its tab — the branch is kept. Close never deletes a worktree unless you
reach for the Shift variant.

## When you're stuck

- The bottom status bar always shows the keys that work _right now_.
- `F1` opens help for whatever has focus; `/` searches all of it.
- `Ctrl-g` is the keybind lock: every chord passes through to the pane
  until you press it again — for when a TUI inside a pane needs your keys.
