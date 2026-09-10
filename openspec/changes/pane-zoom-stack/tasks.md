# Tasks

## 1. Per-tab zoom

- [x] 1.1 `center::Grow` and a `Tab::grow` field (not persisted), set in every
      constructor.
- [x] 1.2 `pane_zoom::cycle_or_zoom` cycles the active tab's level; the loop
      local keeps only the sidebar/panel zone zoom.
- [x] 1.3 `pane_zoom::effective_zoom` plus a loop-top derive that recomputes
      the chrome whenever the grown zone changes.
- [x] 1.4 Activation repairs only a stale focus
      (`sidebar_activate::repair_stale_focus`).

## 2. Stack rendering

- [x] 2.1 `CenterTree::Stack` layout carves collapsed one-row bars, via
      `stack_bars` and `activate_stack_member`.
- [x] 2.2 `pane_zoom::grown_tree` returns a stack of every pane with the
      focused one active; `displayed_tree` is shared by relayout, render and
      mouse.
- [x] 2.3 `borders::draw_stack_bars`, drawn by `chrome::render_panes`.

## 3. Input

- [x] 3.1 `overlay::pre_dispatch` hit-tests the displayed tree;
      `MousePre::StackBar` expands the clicked pane.
- [x] 3.2 `pane_zoom::nav_layout`: vertical moves walk the stack while zoomed.

## 4. Docs and validation

- [x] 4.1 Help page: the Zoom section in `terminal-and-panes.md`.
- [x] 4.2 Unit tests for the stack geometry, the Grow cycle, the effective
      zoom, the nav layout, the bar rendering, and the activation round-trip.
- [ ] 4.3 Pre-push gate (`just test`, clippy, smoke).
