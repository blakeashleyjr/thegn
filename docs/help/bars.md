---
id: bars
title: Masthead & status bar
order: 7
contexts: [zone:masthead, zone:statusbar]
actions:
  [
    toggle-notifications,
    notify-dnd-toggle,
    notify-mode-cycle,
    attention-next,
    mark-all-read,
    open-ci,
    open-proxy-dash,
  ]
---

# Masthead & status bar

The top and bottom chrome bars. Both are focusable zones: `Ctrl-↑` from
the top pane row reaches the masthead, `Ctrl-↓` from the bottom reaches
the status bar; `Esc` returns to the center.

## Masthead (top)

The brand block plus the stats cluster: notifications, CI rollup,
merge-queue depth, disk usage, metrics targets. With the bar focused,
`←/→` walks the items and `↵` opens an item's detail popup — `Esc`, `q`,
or a click outside dismisses it.

## Status bar (bottom)

- **Left:** the mode chip and contextual key hints — the keys that work
  _right now_, for whatever owns focus. The hints follow you: sidebar
  keys while the sidebar is focused, section keys while the panel is.
- **Right:** status widgets (activity, host, share/forward state).

The hint strip is the quick reference; this help (`F1`) and the
[[keybindings]] page are the complete one.

## Notifications & attention

The notification cluster carries its own actions (palette-runnable, all
bindable): toggle the notifications view, cycle notification modes, toggle
do-not-disturb, mark everything read, and jump to the next item that needs
you. The CI item opens the CI runs section; the proxy item opens the LLM
proxy dashboard when configured.
