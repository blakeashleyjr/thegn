# Pane zoom: per-tab, stacked, and hit-tested as drawn

## Why

`Ctrl-Alt-z` zoom (tiled → maximized → full-window) had three user-facing
defects:

- **The wrong pane came back zoomed.** Every sidebar/ring activation reset the
  tab's `focused_pane` to its leftmost pane. Zoom follows focus, so leaving a
  zoomed worktree and returning zoomed its _first_ pane. On top of that, zoom
  was a single loop-global flag, so it leaked into every worktree visited.
- **Nothing showed that a tab was zoomed** apart from a statusbar badge. The
  hidden panes left no trace.
- **Mouse selection started in the wrong place while zoomed.** Hit-testing used
  the tab's split rects, while rendering used the zoomed rect. The selection
  anchor, drag clamp, wheel target and pane-app forwarding were all offset, and
  a click could land on a hidden pane or on a hidden split seam.

## What Changes

- Zoom level becomes per-tab state (`Tab::grow`: tiled / maximized /
  fullscreen). It is not persisted, so a restart comes back tiled. The chrome
  follows the active tab's level through one loop-top derive, so every switch
  path picks it up. A full-window tab whose focus moves to chrome shows that
  chrome until focus returns.
- Activation keeps the tab's remembered focus. Only a stale focus id is
  repaired, to the leftmost pane.
- A zoomed tab renders as a zellij-style stack: the focused pane expanded, and
  each sibling collapsed to a one-row title bar above or below it, in tree
  order. Clicking a bar expands that pane. Layout-spec `Stack` nodes use the
  same rendering.
- The mouse hit-tests the displayed tree, which is the stack while zoomed.
- While zoomed, vertical focus moves walk the stack order. Horizontal moves
  keep the split geometry.

## Non-goals

- Persisting zoom across restarts.
- Any change to the sidebar/panel zone zoom.

## Impact

- `crates/thegn-host/src/{center,borders,chrome,session,run}.rs`
- `crates/thegn-host/src/handlers/{pane_zoom,overlay,sidebar_activate}.rs`
- `docs/help/terminal-and-panes.md`
- Spec: `panes` (ADDED requirements).
- e2e: the zoomed frames in specs 07/19/25/26 now show stack bars. Those
  baselines are already known-stale.
