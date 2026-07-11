# Tasks

## 1. Keyboard/action redesign

- [x] 1.1 Unified `d`/`Delete` chooser (`close_or_delete_menu` + dirty
      variant preserving the safety net; `ConfirmCloseWorktrees` choice;
      `request_close_or_delete` + shared `perform_close` in
      handlers/worktree_delete.rs). `X`/`D` removed.
- [x] 1.2 Workspace-remove modal: safe keep-files default + relabel;
      unprompted removal keeps files.
- [x] 1.3 New keys in handlers/sidebar_keys.rs: `r`/F2, `n`, `N`, `b`, `f`,
      `c`, `?`; `s` → sort menu (`sidebar_sort_menu`, tag `sidebar-sort`);
      EmptyHint Enter = new terminal.
- [x] 1.4 Folder/terminal action bodies (handlers/sidebar_actions.rs):
      rename/create-empty/delete folder (optimistic + deferred write + view-
      key pruning), close terminal (live group + DB row).
- [x] 1.5 Context menu v2: RowMenuEntry {key chip, danger, separator} +
      grouped per-kind catalogs; `menu_step` skips separators; render with
      chips/danger/rules in sidebar_view.rs.
- [x] 1.6 `dispatch_sidebar_outcome!` macro: one outcome dispatch shared by
      keyboard and mouse (run.rs net-shrank).
- [x] 1.7 sidebar_help.rs: `?` card + curated statusbar essentials; wired
      into context_hints and the modal render layer.
- [x] 1.8 Vocabulary sweep (Branch from this…, Move to folder…, disk-
      consequence labels; config.toml.example updated).

## 2. Mouse parity

- [x] 2.1 Hit-testing in sidebar_view.rs: `RowHit`/`hit_rows`/`row_at` with
      caret cells, from the renderer's own `build_sidebar` pass; menu_rect
      anchoring fix (scroll-aware).
- [x] 2.2 handlers/sidebar_mouse.rs: press (caret/Ctrl/double-click/drag
      arming), right-click menu, open-menu click/wheel ownership, drag state
      machine (`Pressed`→`Dragging`, spot resolution, edge autoscroll), pure
      `on_release` + `perform_drop` reusing move/file/unfile machinery.
- [x] 2.3 Drag feedback: `FrameModel.sidebar_drag` + insertion rule/target
      highlight/source lift in sidebar_view.rs.
- [x] 2.4 run.rs wiring: menu interception, right-click branch, drag-move
      coalescing (drain_drag_events), release hook; sidebar wheel rides D5
      damage; SGR mouse gated on `termcaps.mouse`.

## 3. Tests

- [x] 3.1 sidebar_mouse: spot resolution (reorder halves, cross-workspace
      invalid, home anchor, file-into/unfile), Pressed→Dragging threshold,
      release outcomes.
- [x] 3.2 chrome_tests: hit_rows/row_at round-trip (two-line rows).
- [x] 3.3 sidebar_help: coverage + statusbar brevity invariants.

## 4. Validation

- [x] 4.1 Gates green individually (see stabilize-sidebar-internals 5.1);
      e2e baselines regenerated off the June-19 dashboard-era frames; the
      volatile-stats mismatch class is fixed suite-wide (normalize rules),
      residual e2e flake is live-app-state nondeterminism — see
      stabilize-sidebar-internals task 5.3.
- [ ] 4.2 Live TUI pass: right-click menus per row kind, wheel, drag
      reorder/file/unfile + restart persistence, double-click, `d` chooser
      clean/dirty, `?` card, sort menu; right-click passthrough inside an
      htop pane; `TERM=linux` shows no mouse garbage.
