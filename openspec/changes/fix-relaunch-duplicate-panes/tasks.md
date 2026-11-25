## 1. Recorded-session filter

- [x] 1.1 Add the pure helper `recorded_daemon_sessions` in
      `handlers::worktree_attach`: daemon session ids a `pane_sessions`
      entry names whose key is a live leaf of its tab's tree, across every
      group and tab.
- [x] 1.2 Filter the drain's attach targets against it beside the existing
      `shown` re-dedup in `drain_specs`.
- [x] 1.3 Apply the same filter in the prewarm-skip adopt path
      (`attach_prewarm_sessions`).
- [x] 1.4 Unit-test the helper: live-leaf records only (stale-leaf and
      remote-provider records do not filter), and every group/tab is
      covered.
- [x] 1.5 Regression-test the drain: a recorded session restored from a
      relaunch batch reattaches once with no surplus pane and no new split;
      a recordless headless target still takes the missing leaf; a
      prewarm-skip batch with a recorded target grafts nothing.

## 2. Verification

- [x] 2.1 Run `just quick thegn-host` and the focused nextest filters
      (`drain_specs`, `recorded_daemon_sessions`). Covered by the alpha.134 Mac gate (23739 tests, 5/5) on 2026-11-25.
- [ ] 2.2 Reproduce the clip flow live: launch `thegn`, run the tick loop,
      quit (`Ctrl+Q`, `y`), relaunch `thegn` — the restored tab shows one
      pane per recorded session, same tree shape, no duplicate pane.
- [x] 2.3 Run `just ci` (includes openspec-validate). Covered by the alpha.134 Mac gate (23739 tests, 5/5) on 2026-11-25.
