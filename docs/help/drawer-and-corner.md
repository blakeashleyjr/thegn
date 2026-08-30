---
id: drawer-and-corner
title: Drawer & corner pin
order: 6
contexts: [zone:drawer, zone:corner]
actions:
  [
    files-drawer,
    drawer-cycle,
    drawer-pick,
    toggle-corner,
    toggle-strip,
    grow-strip,
    shrink-strip,
    promote-pin,
    unpin,
    pin-1,
    pin-2,
    pin-3,
    pin-4,
    pin-5,
    pin-6,
    pin-7,
    pin-8,
    pin-9,
  ]
---

# Drawer & corner pin

Two auxiliary PTY surfaces outside the center pane tree.

## The files drawer

`Ctrl-Alt-f` (or `Alt-y`) toggles a bottom drawer running a **file
manager**, scoped to the focused worktree. While it owns focus, keys go
straight to the manager; `Ctrl-↑` moves focus back up to the center.
Opening a file from the manager opens it in your `$EDITOR`.

The file manager is a provider seam (`[drawer] kind`). **yazi** is the
default and the fully integrated one — a private, seeded config home, an
accent-matched theme, git status as a linemode, an image-preview
containment policy, and the control channel that lets `q` close the drawer
and `Ctrl-e` open the hovered file. Set `[drawer] kind = "custom"` (or just
`[drawer] command = "lf"`) to run any other manager as a plain contained
pane — it loses those yazi-only integrations, and the host toggle keybind
still closes it. `lf` and `broot` are reserved names for a future build.
Whatever the kind, the drawer's pooling, prewarm, per-worktree open flag,
layout and memory containment behave the same; `thegn doctor` reports the
selected manager (binary availability, config-home mode, caps).

The drawer registry extends `[[tools]]`: set `drawer_scope = "worktree"` for
a tool with one pane per worktree, or `drawer_scope = "global"` for one pane
pooled across worktree switches during this thegn process. `drawer_cwd` is
optional; it is relative to the worktree for worktree tools and must be an
absolute or `~`-prefixed path for global tools. The existing `name`, `command`,
and `env` fields remain the launch configuration. Invalid entries are omitted
without hiding the built-in files occupant.

`drawer-cycle` advances through the files occupant and eligible configured
tools in config order. `drawer-pick` opens a searchable picker by name; Enter
selects the highlighted occupant and Esc cancels without changing state. The
selected occupant is remembered independently for each scope, and a live pane
is pooled according to `[drawer].pool_limit` while switching worktrees. Global
panes are process-local and do not survive detach or restart.

## The corner pin

`Ctrl-Alt-o` toggles a small corner overlay pane — a persistent spot for
something you glance at (an `mpv --vo=tct` player, a log tail). It sits
outside the spatial focus graph: toggle it to focus it, toggle again to
dismiss.

## Pinned programs

`Ctrl-Alt-1..9` launches or focuses `[[pins]]` daemon programs from your
config — long-running tools that live in the top strip and survive tab
switches. See the [[config-reference]] `[[pins]]` section.

`Ctrl-Alt-<digit>` has no legacy terminal encoding, so it only arrives on a
terminal that reports modified keys; `thegn doctor` says whether yours does,
and [[terminal-compatibility]] covers the fix and how to rebind
`summon-pin-1` … `-9`.
