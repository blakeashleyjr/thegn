# Design

## Architecture: daemon owns PTYs, UI is a client

Today `superzej-host` is a foreground compositor: it spawns `portable-pty`
panes, renders chrome, and on exit relies on `session.rs`
(persist/resurrect) to rebuild from the SQLite DB on next launch — the live
PTYs are lost. This change moves PTY ownership into a **long-lived daemon**:

- The daemon owns the `portable-pty` panes and their `PaneEmulator` (vt100)
  state, keyed by worktree/tab/pane, and registers itself in the DB so any
  client can discover it.
- The compositor (and the CLI, and thin remote/mobile clients) become
  **clients** that attach to the daemon over the AK control API
  (445 HTTP/gRPC + 451 SSE/WebSocket event feed + 452 auth scopes/tokens).
- **Warm-reattach**: a client detaches without killing panes; a reattaching
  client receives the current emulator screen as an initial snapshot, then a
  live delta stream — so a running agent is rejoined mid-flight.
- `session.rs` resurrection still applies when no daemon is running (cold
  start). git remains the source of truth for worktrees; the DB is still a
  cache + resurrection layer, now also a daemon/lease registry.

## Rendering & event loop

The damage-region compositor and its `render_plan::plan()` decisions are
**unchanged in shape**; only the _source_ of pane bytes changes — from a local
PTY reader thread to the daemon's delta stream over the control API.

- **Skip** — an idle wake (no pane delta, no chrome/geometry change) still maps
  to `Skip`. The daemon being attached does not generate idle traffic; the relay
  lease and daemon registry never tick the UI loop.
- **Panes** — a pane-content delta arriving from the daemon (warm-reattach
  snapshot apply, or live agent output) marks only `dirty_panes` for the
  affected pane and maps to `Panes` (bounded `Surface::diff_region`). It MUST
  NOT recompose chrome, exactly as a local PTY byte does today.
- **Full** — only chrome/overlay/geometry changes (e.g. a pairing/approval
  overlay, attach/detach status in the statusbar) map to `Full`.

**Wake path (the 0%-idle contract is preserved):** the control-API client
(daemon event stream, SSE/WebSocket, relay reconnect) runs **off the loop** on
the tokio runtime. Each inbound frame is pushed onto the existing tokio **mpsc**
channel and then **pulses the `TerminalWaker`** — identical to PTY reader
threads, model hydration, and the fs-watchers today. The loop continues to block
on termwiz `poll_input(None)` with **no tick and no polling timeout**; it drains
the channel on wake and re-renders only when dirty. No network I/O, lease
bookkeeping, or daemon RPC ever runs on the render loop.

## Persistence

SQLite schema change → **`user_version` bump** (next free version; additive,
non-destructive). Add:

- `daemons` — `id, pid, socket/endpoint, host, started_at, last_heartbeat,
scope` — registry of running daemons so a client can discover and attach;
  heartbeat is written off-loop and never polled by the UI.
- `session_leases` — `id, session_id, daemon_id, client_id, kind
(attached|relay), lease_expires_at, created_at` — records the grace-period
  lease that keeps a remote PTY alive while no client is attached (772);
  resumed/refreshed on reconnect, reaped on expiry by the daemon (not the loop).
- `pairings` — `id, token_hash, scope, label, created_at, expires_at,
revoked_at` — pairing-URL credentials and AK 452 auth scopes/tokens for thin
  clients (771/773). Tokens are stored hashed; never plaintext.

git stays the source of truth for worktrees; these tables are cache/coordination
state, regenerable from running daemons + git. CRUD lives in `superzej-core`
`db.rs` with the 95%-line unit-test gate.

## API, relay, and clients

- **Control API (445/451/452):** the daemon exposes attach/detach, list
  sessions/worktrees, send-to-terminal, snapshot, and drive-browser verbs over
  HTTP/gRPC, with an SSE/WebSocket event feed for pane deltas + activity, gated
  by scoped tokens. `szhost` CLI verbs (770) are thin API callers (extends the
  454 headless CLI) and degrade gracefully when no daemon is running.
- **Relay (772):** when the last client detaches, the daemon opens a
  grace-period **lease** instead of tearing down the PTY; on reconnect within the
  lease the client resumes the same emulator state (warm). Lease expiry reaps the
  PTY. The relay is transport, off-loop, AI-free.
- **Thin clients (771/773):** `szhost serve` advertises a **pairing URL**; a
  desktop/web/mobile client pairs (token in `pairings`) and attaches over the API.
  The **mobile companion** (773) is read-mostly (monitor agents, view activity,
  receive AI 422/423 push) and can stage/commit via the GitBackend seam and
  switch accounts/scopes; all control flows through the same scoped API, so the
  shell never hard-depends on any AI layer.

## Invariants

- **CRITICAL — the ~0% idle event-loop contract is preserved.** The daemon owns
  PTYs, but the UI loop still blocks on termwiz `poll_input(None)` with **no
  polling timeout**. All daemon/relay/API I/O is off-loop on tokio, delivered via
  the existing **mpsc + `TerminalWaker` pulse**; the loop drains on wake and
  re-renders only when dirty. An idle attached client wakes the loop zero times.
- **Render-plan invariants hold:** idle wake ⇒ `Skip`; daemon pane delta and
  nothing else ⇒ `Panes` (never recompose chrome); chrome/overlay/geometry ⇒
  `Full`. The existing `render_plan::plan` unit tests stay the regression gate.
- **AI-free substrate.** Daemon, API, relay, and clients have no AI dependency;
  agents are additive consumers of these seams. The shell must build and run with
  the AI layers absent.
- git remains the source of truth for worktrees; new tables are cache +
  coordination state. No blocking I/O (git, DB, subprocess, network) on the loop.
