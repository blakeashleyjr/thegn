# Tasks — multiplexer parity

## 1. CenterTree geometry ops (thegn-host)

- [x] 1.1 `center.rs`: `resize(pane, dir, step)` — nearest matching-axis
      ancestor, weight shift, minimum-share clamp; no-op result signaled to
      the caller (`ResizeOutcome::{Resized,AtLimit,NoTarget}`).
- [x] 1.2 `center.rs`: `swap(pane, dir)` using the existing focus-neighbor
      geometry walk; leaf exchange keeps slot weights; whole-stack-swaps rule
      (a stacked member moves as its whole `Stack` node).
- [x] 1.3 Exhaustive unit tests: clamps, single-pane no-op, nested splits,
      stacks, focus/swap agreement.

## 2. Keyboard actions

- [x] 2.1 `keymap.rs`/`keymap_specs.rs`: `resize-left/right/up/down`
      (`Ctrl+Shift+arrow`), `swap-pane-left/right/up/down` (`Alt+Shift+hjkl`)
      with default chords + palette entries.
- [x] 2.2 Thin handler arms in a new `handlers/pane_geometry.rs`; route
      mutations through the in-loop `Session` (a `SessionHandle` drop-in when
      add-runtime-session-split lands) and reuse the debounced tab-layout persist.
- [x] 2.3 Claim the new action ids with real prose in
      `docs/help/terminal-and-panes.md` (help + prose ratchets).

## 3. Mouse: border drag-resize

- [x] 3.1 Pure hit-test: `pane_drag::border_at` (border segment → adjacent
      panes + axis); unit tests over laid-out rects.
- [x] 3.2 Drag grab state in the run loop; motion → weight nudge with the same
      clamps; single persist on release; Esc cancels.
- [~] 3.3 Pane-content mouse forwarding is untouched by construction (gestures
  bind only to frame cells; pointer capture ORs into `should_forward_to_pane`)
  and drag frames set chrome damage ⇒ `render_plan::Full` via the _existing_
  invariant. No NEW render_plan unit test was added (the pure `plan()`
  function is unchanged); e2e not exercised (known-broken in this repo).

## 4. Mouse: drag-and-drop rearrange

- [x] 4.1 Pure drop-target resolution: `pane_drag::resolve_drop` →
      `Swap(target)` / `Anchor(target, side)` / `None`; unit tests incl. edge
      bands and drop-on-self.
- [x] 4.2 Lift/hover/commit/cancel wiring + drop highlight rendering
      (`borders::draw_drop_highlight`, theme roles + active glyph set, ascii
      fallback via `active_glyphs`).
- [~] 4.3 Commit path reuses the task-1 tree ops (`swap` / new `anchor`). e2e
  spec NOT recorded — the e2e harness is known-broken/stale in this repo, so
  a snapshot re-record would gate nothing; deferred.

## 5. Daemon recording (`sessions.record`)

- [x] 5.1 `thegn-core`: `Verb::RecordSession` + catalog row `sessions.record`
      (surfaces Http/Grpc/Cli; gRPC listed as a `SURFACE_GAPS` entry, proto
      unimplemented like `sessions.split`); catalog coverage tests green.
- [x] 5.2 Asciicast v2 writer (`thegn_core::asciicast`: header, `o`/`r`/`m`
      events, finalize, size tracking, shared `Utf8Carry`) — pure core with
      unit tests (95% gate applies).
- [x] 5.3 `daemon/session.rs`: `SessionMsg::Record` start/stop/status; tee in
      `on_output` (single null-check when off), buffered `BufWriter` writes,
      `[recording] max_bytes` finalize, stop-on-exit, tombstone carries the path.
- [x] 5.4 Control wire types (`RecordSpec`, `RecordStatus`,
      `SessionInfo.recording`) + HTTP route `POST /v1/sessions/{s}/record` from
      the `ROUTES`/`API_CALLS` tables + regenerated `docs/api/control-v1.json`.
- [x] 5.5 CLI `thegn session record <id> [--stop] [--status] [--json]`;
      no-daemon degradation via the shared `connect()` (json → `{"error":"no_daemon"}`).
- [~] 5.6 Config `[recording] dir`/`max_bytes` documented in
  `config/config.toml.example`; prose in `docs/help/daemon-and-sessions.md`.
  **Recording chip DEFERRED**: the compositor keeps no per-pane `SessionInfo`
  and does not subscribe to its sessions' recording state, so an attached-UI
  chip needs a new client-side data path — it belongs with
  add-runtime-session-split's session-model streaming. Recording stays
  auditable via `session record --status`, `session list` (`recording`
  field) and the tombstone. See the report's deviations.

## 6. Replay cast export

- [x] 6.1 `Recording::export_cast` replays the retained ring through the
      task-5.2 writer (times rebased to 0, geometry from the earliest retained
      event); the toast reports path + covered span (bounded-tail honesty).
- [x] 6.2 Replay overlay `e` key (+ `export-cast` palette action);
      disabled-replay / empty-ring error names `[replay]`; help prose added.

## 7. Gate

- [~] 7.1 Full `just ci` NOT run: the box was saturated (70+ concurrent cargo
  builds across agents) and `just ci` is a full-workspace gate the PreToolUse
  guard refuses. Ran scoped validation instead: `thegn-core` tests green
  (asciicast/config/capability/control, incl. 95%-gated new logic);
  control-v1.json snapshot regenerated; `thegn-svc`/`thegn-host` scoped
  checks + help/keymap ratchets to be run once the box frees.
