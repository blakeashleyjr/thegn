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
    open-usage,
    select-topbar,
    select-bottombar,
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
- **Far right:** the always-on **daemon/status indicator** — a single glyph,
  no label. It is the one badge that is never silent, since it is a persistent
  affordance rather than an alert.

The hint strip is the quick reference; this help (`F1`) and the
[[keybindings]] page are the complete one.

## Daemon / status indicator

Pinned to the far right of the status bar, one glyph reports how this
instance relates to the pane daemon:

- `○` **non-persistent** — the focused pane runs inline; quitting ends it.
- `◆` **persistent** — the focused pane is daemon-backed: quitting detaches
  it (the process keeps running) and the next launch reattaches.
- `▲` **server** — this instance's daemon is serving remote thin clients.
- `▽` **client** — attached to a pane daemon on another machine.

(On ASCII terminals these degrade to `o`, `*`, `^`, `v`.) Activating it — a
click, or `↵` with the bar focused — opens the **status modal**: daemon PID,
version, uptime, live session/attached-client counts, this process's memory
and CPU history, the pane daemon's memory, and the event-loop rollup
(wakes/s, render latency, idle ratio).

## Notifications & attention

The notification cluster carries its own actions (palette-runnable, all
bindable): toggle the notifications view, cycle notification modes, toggle
do-not-disturb, mark everything read, and jump to the next item that needs
you. The CI item opens the CI runs section.

The inbox `⚑`/`✉` chip, the needs-you `✋` chip, and the merge-queue chip
all open the **one unified surface** — a single grouped list of _Needs
you · Alerts · Merge queue · Notifications · Logs_. One clear/dismiss
convention holds throughout: `x` dismisses the row under the cursor, `a`
clears all. Merge-queue rows act in place — `l` lands a gated-green
branch, `r` retries a blocked one, `x` removes it, `m` jumps to the full
Work ▸ Merge queue section. Ephemeral confirmations ("Landed", "Text
copied") surface as transient toasts, the passing view of the same
routed events that also land in this inbox.

## AI account usage

**AI account usage** (command palette, or bind the `open-usage` action) opens
an overlay of per-account rate-limit windows — session / weekly / … — for your
AI coding harnesses (Claude, Codex, Antigravity), drawn as usage bars with a
`used %` and a "resets in …" countdown. It only reads credentials the harnesses
already wrote locally; thegn never asks for or stores an API key. Codex usage is
read offline from its rollup files; Claude and Antigravity don't persist their
windows to disk, so they require `[usage] allow_network = true` (an opt-in,
authenticated request using the on-disk OAuth token) and otherwise show
"unavailable". Configure it under `[usage]`.
