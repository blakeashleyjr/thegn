---
id: terminal-and-panes
title: Terminal & panes
order: 4
contexts: [zone:center]
actions:
  [
    new-tab,
    new-pane,
    split-down,
    split-right,
    close-pane,
    zoom,
    sync-panes,
    next-tab,
    prev-tab,
    copy-pane,
    toggle-key-lock,
    scroll-up,
    scroll-down,
    lazygit,
    yazi,
    editor,
    show-diff,
    save-layout,
    apply-layout,
    export-layout,
    import-layout,
    detach,
    quit,
    quit-kill,
    focus-left,
    focus-right,
    focus-up,
    focus-down,
  ]
---

# Terminal & panes

The center is a real terminal multiplexer: each worktree tab holds a tree
of PTY panes.

## Tabs

- `Alt-t` — new tab on the _same_ worktree; `Alt-T` — a standalone
  terminal tab (no worktree).
- `Alt-←/→` — previous / next tab within the worktree.

## Splits

- `Alt-p` — smart split (along the pane's longer dimension)
- `Alt-n` / `Alt-N` — split down / split right
- `Ctrl-←/↓/↑/→` (or `h/j/k/l`) — move focus between panes and out to the
  [[sidebar]] / [[panel]]
- `Ctrl-Alt-z` — zoom the focused pane; cycles tiled → maximized → full-window
- `Ctrl-Alt-y` — sync panes: broadcast typed input to every pane in the tab

## Tools, scoped to the focused worktree

- `Alt-g` lazygit · `Alt-e` `$EDITOR` · `Alt-/` git diff
- `Alt-y` / `Ctrl-Alt-f` — the bottom files drawer (see [[drawer-and-corner]])

## Copy, search, replay

- `Ctrl-Alt-c` (or `Ctrl-Shift-c`) — copy mode: keyboard selection,
  auto-copy on release; mouse drags select and copy too.
- `Ctrl-Alt-/` — search the focused pane's history; `Ctrl-/` searches
  across panes.
- `Alt-r` — time-travel replay of the focused pane (needs `[replay]`
  enabled).

## When keys collide

`Ctrl-g` locks the keymap: every chord passes through to the pane until
pressed again. Use it when a TUI inside the pane needs chords thegn owns.

## Layouts

Named layouts snapshot a tab's pane tree: save, apply, export, and import
them from the [[command-palette]].
