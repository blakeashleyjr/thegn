# Tasks — drag grab precision (THE-67)

- [x] Add pure separator grab/follow geometry and tab-chip gap coverage.
- [x] Make separator presses inert until motion, retain grab offset, persist only
      moved releases, and restore snapshots on Escape.
- [x] Use pane-padding-aware resize slop without stealing content clicks.
- [x] Focus a pane on a motionless frame click and preserve rearrange behavior
      after motion.
- [x] Resolve an in-sidebar row drop to the nearest row and cancel outside.
- [x] Update sidebar, panel, and terminal/pane help prose.
- [x] Run the recorded focused host, pane-drag, border, drop, and strict OpenSpec
      gates; implementation is captured by the THE-67 merge history.

## Validation boundary

No visual baseline re-record or standalone historical `just ci` result is
claimed. Those gate-only reminders were not part of the accepted fix evidence.
