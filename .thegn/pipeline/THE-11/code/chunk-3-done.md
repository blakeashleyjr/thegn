# Chunk 3 completion — chrome indicator, loop wiring, and help

## Delivered

- Wired the chunk-2 drawer registry/runtime into the host loop for files toggle,
  cycle, picker selection, stale-result draining, process exit, and geometry
  invalidation.
- Added the pure `FrameModel` drawer-bar snapshot and an atomic, shared
  paint/hit-test statusbar item using the folder glyph and existing theme
  colors. The item reports the active occupant and valid-occupant count and is
  removable through `[bars].bottom_left`.
- Added `drawer-cycle` and `drawer-pick` help/action documentation, including
  worktree/global scope, pooling, persistence, and drawer metadata behavior.
- Added focused statusbar tests for atomic painting/hit testing and removal.

## Verification

- `XDG_RUNTIME_DIR=/tmp/tg-the11-runtime RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= just quick thegn-host` — passed.
- `cargo fmt --all` — passed.
- `git diff --check` — passed.
- Help ratchet files remain valid and empty of debt; the env-overlay metadata
  pins are unchanged. Completion-slot and control-schema surfaces were not
  changed.
- No e2e run, snapshot re-recording, built-binary invocation, migration, or
  live state-DB access was performed.

## Unverified

- The requested `cargo nextest run -p thegn-host statusbar` targeted run could
  not compile the host test target because chunk-2 `drawer_state.rs` references
  `crate::panes::PANE_EVENT_CHANNEL_CAPACITY`, which is absent/private there
  (`E0425`). The `chrome`, `drawer`, and `help` filters are consequently also
  unverified for the same compile blocker.
- The eight affected chrome/bar snapshots were not re-recorded:
  - `test/muse/snapshots/chrome_regions__chrome/xterm__100x30__linux.txt`
  - `test/muse/snapshots/chrome_regions__chrome/xterm__160x40__linux.txt`
  - `test/muse/snapshots/chrome_regions__chrome/xterm__200x50__linux.txt`
  - `test/muse/snapshots/chrome_regions__chrome/xterm__40x12__linux.txt`
  - `test/muse/snapshots/chrome_regions__chrome/xterm__80x24__linux.txt`
  - `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__100x30__linux.txt`
  - `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__160x40__linux.txt`
  - `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__80x24__linux.txt`
