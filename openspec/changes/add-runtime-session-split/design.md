# Design

## The decoupling boundary — draw the line at the emulator grid, not the loop

**Stays client-side:** termwiz `BufferedTerminal`/`Surface`/`TerminalWaker`,
`poll_input`, input decoding, terminal caps, the damage-region compositor +
`render_plan::plan`, and **the per-pane vt100 emulator grid** (`emulator.rs`).
This is the load-bearing decision: the client keeps applying pane bytes into a
local grid and composing from it, so a daemon-backed pane tick is
indistinguishable from a local PTY tick (`PaneIo::Stream` already works this way)
and stays a one-rect `Incremental` frame — the work-shape the CI render tests
lock.

**Moves into the daemon:** the session model (`Session`/`WorktreeGroup`/`Tab`/
`CenterTree` — layout, focus, active group/tab), pane→session-id ownership,
whole-session activity/lease state, and `Session::persist`/`resurrect`
(single-writer; the DB stays a cache, git stays the source of truth).

**Per-client even after the split:** chrome is _rendered_ client-side from the
streamed model + local view state, so different-sized clients render different
chrome from one session. Cursor/selection/scroll are local.

## Protocol — stream a semantic model, not a framebuffer

Streaming a composed framebuffer would push a whole-screen diff per daemon-side
pane tick and force one geometry — breaking both the render invariants and
multi-geometry clients. Instead extend `EventFrame` with `SessionModel { json }`
(full snapshot; `CenterTree` already `Serialize`), `LayoutDelta { json }` (one
structural mutation), and `FocusChanged { session, pane }`. Pane bytes keep
riding the existing per-session `attach` streams. Reuse the existing
`AttachKind::Observer`/`Interactive` "last interactive writer wins" rule for
layout authority.

## Carving `run.rs` without a rewrite

The loop reads/writes `session`/`panes` as locals. A `SessionHandle` trait
captures only the ~30 _structural_ mutations; a `LocalSession` shim wraps the
existing `Session` + `center.rs` mutators (Phase 1 is behavior-identical, so
`just ci` stays green with no semantic change). The socket-backed `RemoteSession`
is a later drop-in the loop can't distinguish. The loop's thousands of lines that
_read_ `session.active_tab().center` are untouched; only the mutation call sites
are redirected.

## Risks and mitigations

- **0%-idle across the socket:** the relay task pulses the client waker _only_ on
  real damage-producing frames (pane bytes, this client's `LayoutDelta`/
  `FocusChanged`); heartbeat/lease frames are dropped — the discipline
  `daemon/client.rs::adapt` already follows. CI test: attached-but-quiet remote
  session ⇒ Skip on a spurious wake.
- **Multi-client focus fights:** focus is server-authoritative; `apply_layout`
  from an interactive client wins and is broadcast; each client re-derives.
- **Read-only main checkout:** unchanged — git already routes through the
  daemon's `GitBackend` seam; moving layout server-side doesn't touch it.
- **DB double-writer:** persistence becomes single-writer (the daemon persists;
  clients stop calling `Session::persist`); the `resurrect` tiered-ordering logic
  moves call site, not logic.
- **Loop blocking on RPC:** `SessionHandle` mutations are fire-and-forget onto
  the relay task with results landing as frames + waker pulse (the
  `enqueue_git_op` pattern); the loop never awaits a socket inline.

## What already shipped (context)

The additive agent-driving surface this generalizes is already in place: local
`thegn attach`, the `wait`/`split` control verbs, per-pane semantic agent-state
core, and the in-app diff viewer. The pane daemon is already the default
(`[daemon] enabled = true`), so center panes already survive UI exit and
warm-reattach at next launch. This change makes the _whole session_ (layout +
focus) daemon-owned so live multi-client attach and a headless owner become
possible.
