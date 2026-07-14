---
id: drawer-and-corner
title: Drawer & corner pin
order: 6
contexts: [zone:drawer, zone:corner]
actions:
  [
    files-drawer,
    toggle-corner,
    toggle-strip,
    grow-strip,
    shrink-strip,
    promote-pin,
    unpin,
  ]
---

# Drawer & corner pin

Two auxiliary PTY surfaces outside the center pane tree.

## The files drawer

`Ctrl-Alt-f` (or `Alt-y`) toggles a bottom drawer running the bundled
**yazi** file manager, scoped to the focused worktree. While it owns
focus, keys go straight to yazi; `Ctrl-↑` moves focus back up to the
center. Opening a file from yazi opens it in your `$EDITOR`.

## The corner pin

`Ctrl-Alt-o` toggles a small corner overlay pane — a persistent spot for
something you glance at (an `mpv --vo=tct` player, a log tail). It sits
outside the spatial focus graph: toggle it to focus it, toggle again to
dismiss.

## Pinned programs

`Ctrl-Alt-1..9` launches or focuses `[[pins]]` daemon programs from your
config — long-running tools that live in the top strip and survive tab
switches. See the [[config-reference]] `[[pins]]` section.
