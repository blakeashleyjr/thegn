# Tasks

- [x] Restrict the busy glyph cadence to visible, recently touched, pane-bearing,
      or agent-bearing worktrees; use the quiet safety TTL for other registry rows.
- [x] Raise the default background busy glyph refresh interval to 30 seconds.
- [x] Record per-pass considered, reused, and scanned glyph row counts.
- [x] Bound concurrent git glyph scans to two workers.
- [x] Scope periodic CI refresh to the focused worktree.
- [x] Apply configured PR and issue cache TTLs to periodic provider refreshes.
- [x] Add a 1,000-worktree idle CPU benchmark.
- [ ] Verify on the remote Mac with `just bench-idle-many-worktrees`.
