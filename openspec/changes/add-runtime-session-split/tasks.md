# Tasks

Each phase is independently shippable and MUST keep `just ci` green — in
particular the `render_plan::plan` invariant tests and the ~0%-idle contract.

## Phase 1 — `SessionHandle` seam (pure refactor, no behavior change)

- [ ] 1.1 Add `crates/thegn-host/src/session_handle.rs`: `SessionHandle` trait
      (`split`/`close_pane`/`focus`/`focus_move`/`add_tab`/`close_tab`/
      `switch_group`/`switch_tab`/`open_pane`, `snapshot() -> SessionModel`,
      `subscribe()`), plus a `LocalSession` impl wrapping today's `Session` +
      `center.rs` mutators, behavior-identical.
- [ ] 1.2 Route `run.rs`'s ~30 structural-mutation `Action::*` sites through the
      handle; pane _bytes_ stay on the existing `PaneEvent → emulator → dirty_panes`
      path (never through the handle).
- [ ] 1.3 Verify `render_plan` invariant tests unchanged; add a test that a
      handle-routed split still yields a Full frame.

## Phase 2 — Semantic session protocol

- [ ] 2.1 `control_wire::EventFrame`: add `SessionModel { json }`,
      `LayoutDelta { json }`, `FocusChanged { session, pane }`; extend the
      encoder/decoder and every exhaustive `EventFrame` match (adapters).
- [ ] 2.2 `ControlApi`: add `session_model`/`apply_layout`/`subscribe_layout`/
      `attach_session` with default impls (no gRPC/proto churn); HTTP routes +
      `Verb`s + scopes + exhaustiveness test.
- [ ] 2.3 Client + CLI plumbing for the new verbs.

## Phase 3 — `RemoteSession` (daemon owns layout/focus/persist)

- [ ] 3.1 Daemon owns a `SessionModel` per state dir (`daemon/service.rs`),
      applying `apply_layout`, broadcasting `LayoutDelta`/`FocusChanged`.
- [ ] 3.2 `RemoteSession: SessionHandle` in the host: mutations become
      `apply_layout` RPCs; a relay task turns deltas into loop wakeups (waker +
      chrome damage → Full). Filter heartbeat/lease frames so a quiet stream
      never wakes an idle client.
- [ ] 3.3 Move `session.rs` `persist`/`resurrect` behind the daemon (single
      writer; DB stays a cache); clients stop writing layout to SQLite.
- [ ] 3.4 Tests: delta-induced repaint is Full; idle layout stream is Skip
      (extend `idle_attached_daemon_wake_skips`).

## Phase 4 — Whole-session attach + headless owner

- [ ] 4.1 `attach_session`/`detach_session` verbs; extend the relay lease
      machinery (`daemon/mod.rs::lease_loop`) from per-PTY to per-session-group.
- [ ] 4.2 Flip the default `SessionHandle` to `RemoteSession`; the daemon owns
      the session with zero clients; the compositor loop is a pure client.
- [ ] 4.3 Multi-client focus authority ("last interactive writer wins");
      per-client cursor/selection stays local.

## Validation

- [ ] Run `just ci` (pre-PR gate) after each shippable phase.
- [ ] Manual: `just start name=dev`, split/close/focus with the local handle;
      then attach a second client and confirm a split in one shows in the other;
      quit every client and confirm panes keep running and reattach whole.
