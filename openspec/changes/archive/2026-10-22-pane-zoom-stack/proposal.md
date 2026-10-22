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

- Runtime repair owner: [THE-559](https://linear.app/blakeashley/issue/THE-559),
  Workspace Product & Observability. Delivery registration/support owner:
  [THE-560](https://linear.app/blakeashley/issue/THE-560), Spec & Tracker
  Reconciliation. Both are required before merge: THE-560 restores source
  gate eligibility, and THE-559 supplies the visibility repair and focused
  evidence. Neither replaces integrated/full-gate or visual verification.
- Lifecycle remains active and verification-gated; the original queued branch
  and this repair do not establish completion of the remaining gates.
- Full-feature review also requires THE-566 (keyboard focus must activate a
  saved stack before restoring tiled), THE-567 (unpaintable narrow bars must
  not become click targets), and THE-568 (paired geometry must not duplicate
  layout walks or allocate discarded outputs), all owned by Workspace Product
  & Observability. These depend on the shared visibility/caller foundation in
  THE-559 and delivery registration in THE-560; all must pass their focused
  regressions before the common integrated/full/visual gate.

- `crates/thegn-host/src/{center,borders,chrome,session,run}.rs`
- `crates/thegn-host/src/handlers/{pane_zoom,overlay,sidebar_activate}.rs`
- `docs/help/terminal-and-panes.md`
- Spec: `panes` (ADDED requirements).
- e2e: the zoomed frames in specs 07/19/25/26 now show stack bars. Those
  baselines are already known-stale.
