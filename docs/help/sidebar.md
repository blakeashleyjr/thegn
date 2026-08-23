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
`Ctrl-Alt-s` cycles it through three states: **full → rail → hidden**. The
rail is a slim strip that keeps each row's activity dot and initial visible
while reclaiming the columns; press `Alt-s` to grow it back into the full
tree. `q` or `Esc` returns to the terminal.

## Navigate

- `↑↓` / `j` `k` — move; `↵` opens the row (or folds a header; on the
  empty TERMINALS hint it creates a terminal)
- `PgUp` `PgDn` — move by ten rows; `Home` `End` — first / last row
- `←` / `h` — fold a header (from a row inside one: jump to the header);
  `→` / `l` — unfold
- `/` — filter the tree (`↵` applies it; the next `Esc` or `q` clears it)
- `Alt-1..9` / `Ctrl-1..9` — jump to worktree / workspace by slot
- `Alt-\`` — bounce between the workspaces and terminals regions
- `q` or `Esc` — back to the terminal

## Create

- `n` — new worktree in the workspace under the cursor (in the TERMINALS
  section: a new terminal)
- `N` — new workspace
- `b` — branch a new worktree off the one under the cursor

## Organize

- `f` — move to a folder (or create one)
- `r` / `F2` — rename
- `p` — pin to top
- `s` — sort menu: manual / name / recent / attention / live (live orders
  worktrees by most-recent process/agent activity, newest first)
- `Space` — mark rows for bulk actions (marks clear when you click or type
  into a pane); `Shift-↑↓` — reorder manually

## Reorder

`Shift-↑↓` moves the row under the cursor (or every marked row, as a
block); `Ctrl-Alt-↑↓` moves one item and works from anywhere.

A worktree moves within its **run** — the loose list under its workspace,
or the folder it is filed into. Push it past the end of a run and it
crosses into the next one, leaving or joining that folder as it goes: one
key both reorders and re-files. `home` is anchored at the top of the loose
list; it never moves and nothing lands above it. A collapsed folder is
stepped over rather than entered, so a worktree can't vanish into a folder
you have closed.

Put the cursor on a folder header to reorder the folder itself among its
workspace's folders — its worktrees travel with it. Workspace headers and
terminals reorder among their own kind the same way.

Manual reordering implies the manual sort, so a move under a computed sort
(`s`) switches back to manual first. Order is saved per workspace and
restored on the next launch.

## Mouse

Mouse support turns on only when the outer terminal reports it; every
gesture below has a keyboard equivalent.

- **Click** selects the row (and opens a worktree or terminal; headers just
  select); clicking the caret glyph folds or unfolds a header instead
- **Double-click** moves keyboard focus into the pane (or folds a header)
- **Ctrl-click** marks a row for bulk actions, like `Space`
- **Right-click** opens that row's action menu, anchored under it; the menu
  then takes clicks and the wheel until you pick an entry or click away
- **Wheel** scrolls the tree, walking the selection with it (and focuses
  the sidebar, like a click)
- **Drag** a worktree to reorder it. Release **on** a row to put the dragged
  worktree in that row's place — the row you drop on moves aside. Drop on the
  **last** row to land at the end. This works _inside_ a folder too, which
  files and positions it in one go; drop on a folder header to file it at the
  end of that folder, or on its workspace header to move it back out. Drag a
  folder header to reorder folders, or a workspace header to reorder
  workspaces. Dragging across workspaces is refused, `home` stays anchored at
  the top, and the insertion rule shows exactly where a release will land.
  `Esc` abandons a drag without moving anything.

## Act

- `d` / `Del` — close or delete… (deleting files from disk is always the
  explicit second choice, never the default)
- `c` — copy the worktree path
- `m` — the full row action menu

## View

- `<` / `>` (or `,` / `.`) — resize the sidebar; `e` — wide mode (`Esc`
  collapses it back)
- `?` — this page
- `g` — flat / grouped: toggle between one list of every worktree across
  all repos (each tagged with its repo, ordered by the current `s` sort)
  and the per-workspace grouping. Pair with the `s` → live sort to always
  see the latest-changed worktree at the top, regardless of workspace
- `i` — row detail: cycle the secondary line (branch, ahead/behind, PR)
  between **all** rows, the **cursor** row only, and **off**. The detail
  line only ever shows while the sidebar has focus. Defaults to the cursor
  row — **all** doubles every worktree row's height on focus, so it halves
  how much of the tree fits. This overrides
  `sidebar_focus_detail` from [[config-reference]] `[ui]` and persists

Workspace ordering is configurable: `sidebar_workspace_sort = "attention"`
bubbles the workspace that most needs you to the top. See
[[config-reference]] `[ui]`.
