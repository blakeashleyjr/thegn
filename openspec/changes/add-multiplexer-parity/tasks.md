# Tasks — multiplexer parity

## 1. CenterTree geometry ops (thegn-host)

- [ ] 1.1 `center.rs`: `resize(pane, dir, step)` — nearest matching-axis
      ancestor, weight shift, minimum-share clamp; no-op result signaled to
      the caller.
- [ ] 1.2 `center.rs`: `swap(pane, dir)` using the existing focus-neighbor
      geometry walk; leaf exchange keeps slot weights; define the
      whole-stack-swaps rule.
- [ ] 1.3 Exhaustive unit tests: clamps, single-pane no-op, nested splits,
      stacks, focus/swap agreement.

## 2. Keyboard actions

- [ ] 2.1 `keymap.rs`/`keymap_specs.rs`: `resize-left/right/up/down`,
      `swap-pane-left/right/up/down` with default chords + palette entries.
- [ ] 2.2 Thin handler arms in a new `handlers/pane_geometry.rs` (god-file
      guidance: no run.rs growth); route mutations through the session owner
      (in-loop `Session` today; `SessionHandle` when add-runtime-session-split
      lands) and reuse the debounced tab-layout persist.
- [ ] 2.3 Claim the new action ids with real prose in
      `docs/help/terminal-and-panes.md` (help + prose ratchets).

## 3. Mouse: border drag-resize

- [ ] 3.1 Pure hit-test: border segment → (split node, adjacent branches);
      unit tests over laid-out rects.
- [ ] 3.2 Drag state on the frame model; motion → weight delta with the same
      clamps; single persist on release; Esc cancels.
- [ ] 3.3 Verify pane-content mouse forwarding is untouched
      (`mousefilter` tests) and drag frames are chrome damage
      (`render_plan` invariant tests extended).

## 4. Mouse: drag-and-drop rearrange

- [ ] 4.1 Pure drop-target resolution: pointer cell + pane rects →
      `Swap(target)` / `Anchor(target, side)` / `None`; unit tests including
      edge-band boundaries.
- [ ] 4.2 Lift/hover/commit/cancel state machine + drop highlight rendering
      (theme roles + active glyph set; ascii fallback).
- [ ] 4.3 Commit path reuses the task-1 tree ops; e2e spec for
      lift-highlight-drop (re-record with `just e2e-update`).

## 5. Daemon recording (`sessions.record`)

- [ ] 5.1 `thegn-core`: `Verb::RecordSession` + catalog row `sessions.record`
      (surfaces Http/Grpc/Cli); catalog coverage tests.
- [ ] 5.2 Asciicast v2 writer (header, `o` events, `r` resize events,
      finalize) as pure-ish core logic with unit tests (95% gate applies).
- [ ] 5.3 `daemon/session.rs`: record start/stop/status messages; tee in
      `on_output` (null-check-free-when-off), buffered writes off-loop,
      `[recording] max_bytes` finalize, stop-on-exit, tombstone carries the
      path.
- [ ] 5.4 Control wire types (`RecordSpec`, `RecordStatus`,
      `SessionInfo.recording`) + HTTP/gRPC routes from the `ROUTES` table +
      regenerate `docs/api/control-v1.json` snapshot.
- [ ] 5.5 CLI `thegn session record <id> [--stop] [--json]`; no-daemon
      degradation message; smoke-test coverage.
- [ ] 5.6 Recording chip in the attached UI statusbar; document
      `[recording] dir`/`max_bytes` in `config/config.toml.example`; prose in
      `docs/help/daemon-and-sessions.md`.

## 6. Replay cast export

- [ ] 6.1 Export the retained ring through the task-5.2 writer (times rebased,
      geometry from earliest retained event); bounded-tail honesty in the
      toast.
- [ ] 6.2 Overlay key + `export-cast` palette action; disabled-replay error;
      help prose in the replay section.

## 7. Gate

- [ ] 7.1 Run `just ci` once (includes openspec-validate, help ratchets,
      render-plan invariant tests).
