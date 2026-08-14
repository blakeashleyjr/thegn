---
id: terminal-and-panes
title: Terminal & panes
order: 4
contexts: [zone:center]
actions:
  [
    new-tab,
    new-terminal,
    new-pane,
    split-down,
    split-right,
    close-pane,
    zoom,
    redraw,
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
    nav-left,
    nav-right,
    nav-up,
    nav-down,
  ]
---

# Terminal & panes

The center is a real terminal multiplexer: each worktree tab holds a tree
of PTY panes.

## Tabs

- `Alt-t` — new tab on the _same_ worktree; `Alt-T` — a standalone
  terminal tab (no worktree).
- `Alt-←/→` — move focus left / right (see Splits); at the outer edge, with
  no pane or chrome left to focus, it switches to the previous / next tab.

## Splits

- `Alt-p` — smart split (along the pane's longer dimension)
- `Alt-n` / `Alt-N` — split down / split right
- `Ctrl-←/↓/↑/→` (or `h/j/k/l`) — move focus between panes and out to the
  [[sidebar]] / [[panel]]
- `Alt-←/→` — the same seamless horizontal motion, but a move that runs off
  the outer edge falls through to a previous / next tab switch, so one key
  walks the whole row and keeps going
- `Alt-↑/↓` — cycle the previous / next worktree within the current workspace.
  This never focuses the top / bottom bars — that is `Ctrl-↑/↓`.
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

## Fixing a garbled screen

`Ctrl-Shift-l` forces a full redraw — the classic Ctrl-L "fix my screen"
escape hatch. Reach for it if the display drifts (repeated or leftover
lines) after the outer terminal window regains focus: some terminals
repaint their alt screen imperfectly and thegn gets no event to heal it
automatically.

## Layouts

Named layouts snapshot a tab's pane tree: save, apply, export, and import
them from the [[command-palette]].
