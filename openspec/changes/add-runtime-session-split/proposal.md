# Runtime session split (the compositor loop stops owning the session)

## Summary

thegn's pane **daemon** already owns the PTYs, so center panes survive a UI exit
and warm-reattach at next launch (tmux semantics, on by default). But the
**session model** — the pane-tree layout, focus, active group/tab — still lives
as locals inside the compositor's event loop (`run.rs`). The loop *is* the
session owner; a second UI cannot attach to a running local session's layout,
and the loop cannot run headless. Competitor "herdr" makes the runtime the owner
and every UI (TUI, CLI, SSH) a client: work survives *every* client closing, and
many clients can attach to one live session at once.

This change moves session ownership behind a seam so the daemon can own it and
the compositor becomes a renderer, delivered in independently-shippable phases
that each keep the render-decision invariants and the ~0%-idle contract green:

1. **`SessionHandle` seam.** A trait covering the loop's structural mutations
   (split/close/focus/add-tab/switch/open-pane, plus `snapshot()`/`subscribe()`).
   A `LocalSession` impl wraps today's in-loop `Session` behavior-identically;
   `run.rs`'s ~30 structural-mutation sites route through the handle. No
   semantic change — pure seam.
2. **Semantic session protocol.** New `EventFrame::SessionModel`/`LayoutDelta`/
   `FocusChanged` frames (the daemon streams a *model*, not a framebuffer — each
   client composes chrome at its own geometry) and `ControlApi`
   `session_model`/`apply_layout`/`subscribe_layout`/`attach_session` verbs.
3. **`RemoteSession`.** A socket-backed `SessionHandle` whose mutations are
   `apply_layout` RPCs and whose relay task turns layout deltas into loop wakeups
   (waker pulse + chrome damage → a full frame — the sanctioned repaint path).
   Session persistence becomes daemon-owned (single writer; the DB stays a cache).
4. **Whole-session attach + headless owner.** `attach_session`/`detach_session`
   as an atomic unit over per-session-group relay leases; `RemoteSession` becomes
   the default so the daemon owns the session with zero clients attached and the
   compositor loop is a pure client.

## Impact

- Roadmap: `tasks.md` **Group (runtime/client split)** — the keystone joining the
  AI-free shell to the client-agnostic runtime.
- Spec: `control-plane` — ADDED session-model + layout ops + whole-session attach.
  `event-loop` — MODIFIED loop-ownership requirement (delegates via `SessionHandle`).
- Code: new `crates/thegn-host/src/session_handle.rs` (`SessionHandle` +
  `LocalSession`/`RemoteSession`); `control_wire::EventFrame` variants
  (`SessionModel`/`LayoutDelta`/`FocusChanged`); `ControlApi` verbs (default impls
  so no adapter churn beyond the daemon); `daemon/service.rs` session-model owner;
  `session.rs` single-writer persist; `run.rs` structural-mutation sites routed
  through the handle. `render_plan` invariant tests extended (delta-induced Full,
  attached-quiet Skip).

## Rationale

The seam is drawn at the per-pane vt100 emulator grid, not the loop: the client
keeps emulation + the damage-region compositor local, so a daemon-backed pane
tick stays a one-rect `Incremental` frame — the exact work-shape the CI render
tests lock. Streaming a *semantic model* (not pixels) is what lets a phone, an
80×24 SSH client and a 4K terminal each render their own chrome from one session,
and keeps the 0%-idle contract client-local. The additive agent-driving verbs
(`wait`/`split`, local `thegn attach`, per-pane agent state) already shipped; this
change generalizes the daemon's existing PTY persistence to the *whole session*.

## Non-goals

- **Moving emulation server-side.** The per-pane vt100 grid stays client-side;
  moving it would push a full-screen diff per pane tick across the socket and
  break the render invariants.
- **A new transport.** Reuses the existing HTTP/WS control plane + unix socket;
  no new IPC.
- **Multi-client conflict resolution beyond "last interactive writer wins."**
  Focus is server-authoritative; per-client cursor/selection stays local.
