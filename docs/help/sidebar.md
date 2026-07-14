---
id: sidebar
title: Sidebar
order: 3
contexts: [zone:sidebar]
actions:
  [
    focus-sidebar,
    toggle-sidebar,
    move-item-up,
    move-item-down,
    move-worktree-to-folder,
    toggle-region,
  ]
---

# Sidebar

The left tree: every workspace, its worktrees, and your standalone
terminals. `Alt-s` (or `Ctrl-←` from the leftmost pane) focuses it;
`Ctrl-Alt-s` hides it. `q` or `Esc` returns to the terminal.

## Navigate

- `↑↓` / `j k` — move; `↵` opens the row (or folds a header)
- `← →` — collapse / expand
- `/` — filter the tree
- `Alt-1..9` / `Ctrl-1..9` — jump to worktree / workspace by slot
- `Alt-\`` — bounce between the workspaces and terminals regions

## Create

- `n` — new worktree in the workspace under the cursor
- `N` — new workspace
- `b` — branch a new worktree off the one under the cursor

## Organize

- `f` — move to a folder (or create one)
- `r` / `F2` — rename
- `p` — pin to top
- `s` — sort menu: manual / name / recent / attention
- `Space` — mark rows for bulk actions; `Shift-↑↓` — reorder manually

## Act

- `d` / `Del` — close or delete… (deleting files from disk is always the
  explicit second choice, never the default)
- `c` — copy the worktree path
- `m` — the full row action menu

## View

- `< >` — resize the sidebar; `e` — wide mode

Workspace ordering is configurable: `sidebar_workspace_sort = "attention"`
bubbles the workspace that most needs you to the top. See
[[config-reference]] `[ui]`.
