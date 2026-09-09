# Design — make-daemon-default

## Context

The pane daemon shipped with `add-control-plane-and-remote`: `thegn daemon`
(hidden subcommand) owns PTY + emulator + history per session behind a unix
socket, with warm-reattach (snapshot-at-seq + live deltas), a lease reaper, an
idle-exit janitor, and a heartbeat registry. The compositor routes panes
through it when `[daemon] enabled = true` via the single spawn chokepoint
`Panes::spawn_argv_env` → `spawn_daemon_backed` (`crates/thegn-host/src/panes.rs:368-472`),
and resurrection reattaches persisted `provider = "daemon"` session ids
(`panes.rs:616-643`).

Verified properties that make default-on viable:

- **Daemon spawn is lazy and off the critical path** — `LazyDaemonSource::ensure_daemon`
  runs inside the relay task (`daemon/client.rs`), never on the event loop.
  First frame is unaffected; a cold daemon costs ~100-150ms before the first
  prompt only.
- **Sandbox/ssh panes already route through the daemon** — wrapping happens at
  argv-build time, before the chokepoint. Native provider-exec panes bypass it
  and keep their own session persistence.
- **`exit`ing a shell behaves correctly** (child exit → daemon removes session),
  and daemon hygiene exists (socket-as-lock, boot sweep of dead-pid registry
  rows, idle-exit, per-`XDG_STATE_HOME` socket isolation).

Blocking gaps (why it is still opt-in):

1. **Close leaks sessions.** Dropping a `PtyPane` makes the relay send a detach
   (`SessionEnd::PaneGone`, `pane.rs:929`) → the daemon opens a lease and the
   shell keeps running. `ControlClient::kill` exists but the compositor never
   calls it.
2. **Ephemeral panes orphan leases.** Pins, the tool drawer, and the corner
   overlay spawn through the chokepoint but are not in any tab's center tree,
   so their session ids are never persisted — on quit they detach into leases
   nobody reattaches.
3. **Expired reattach = error husk.** `relay_exec` (`pane.rs:785-814`) only
   uses the fallback spec on a post-connect drop; an initial attach failure
   husks the pane (`[native exec failed…]` + Exit(1)).
4. **Reboot fidelity lost.** cwd/foreground-cmd capture is `/proc/<pid>`-based
   and stream panes have `pid: None`; scrollback capture skips daemon panes.

## Goals / Non-Goals

**Goals:**

- Bare `thegn` is tmux: quit keeps center-tree panes running; relaunch lands in
  the same spot with live screens; reboot degrades to fresh shells with
  scrollback tails and relaunch overlays.
- Explicit close means kill; only deliberate walk-aways hold sessions.
- The lifecycle is visible (chip, exit message) and controllable (Detach,
  Quit-and-kill).

**Non-Goals:**

- Daemon supervision / auto-restart with session re-adoption (tasks.md item 8).
- Named saved sessions (item 115); smart lease GC.
- Kill-on-close for provider (sandbox-native) stream panes — same leak shape,
  different ownership; unchanged here.
- Agent harness resume (`add-agent-session-resume`) — complementary; the
  expiry-fallback path built here is where `claude --resume` slots in later.

## Decisions

- **Flip `[daemon] enabled = true` only together with the three gap fixes.**
  Flipping alone ships a leak machine (every closed pane lingers; every quit
  orphans pin/drawer leases). Alternative — keep opt-in and document — rejected:
  persistence is the headline behavior users expect from a multiplexer.
- **`lease_grace_secs = 0` means never reap, and becomes the default.** True
  tmux semantics; "come back tomorrow" must work. With close=kill and the
  ephemeral bypass, leases only hold deliberate walk-aways, all reattached by
  the next bare `thegn`. Alternative — 14-day backstop — rejected by user;
  residual leaks stay visible in `thegn session list`. `idle_exit_secs` stays
  1800 so an empty daemon still exits.
- **Kill-vs-detach is a per-pane drop flag, decided by the compositor.**
  `ExecSource::kill_session` (default no-op, implemented by the daemon sources
  via `ControlClient::kill`) + an `Arc<AtomicBool> detach_on_drop` on stream
  panes, default false (= kill on `SessionEnd::PaneGone`). `Action::Quit` marks
  center-tree panes detached before returning. Alternative — plumb an explicit
  close verb through every `panes.table.remove` site (~15 call sites) —
  rejected: drop-based semantics catch every removal path with one mechanism.
- **Ephemeral bypass at the chokepoint,** via a local-spawn variant that skips
  `daemon_cfg`, used by pins/drawer/overlay. Alternative — persist + reattach
  ephemerals too — rejected: pins are supervised respawn-on-launch panes
  already; persistence adds lease bookkeeping for zero user value.
- **Initial-attach failure falls back in the relay, not the loop.** `relay_exec`
  already holds the `fallback`/`reopen_spec`; on initial `ExecOpen::Attach`
  failure it calls `source.open(&fallback)` and emits
  `PaneEvent::SessionFallback(pane_id)` so the loop repaints the persisted
  scrollback tail and arms the relaunch overlay (mirroring the host-pane path,
  `panes.rs:731-756`). Loop-side handling lives in a new
  `src/handlers/daemon_lifecycle.rs` — run.rs is ratchet-pinned and its match
  arms stay thin calls.
- **Reboot fidelity by exposing the daemon child pid** in `SessionInfo` / the
  attach `Hello` (`thegn-svc/src/control` types) and setting `PtyPane.pid` for
  daemon panes: `/proc`-based cwd/cmd capture then works unchanged (same-host
  child). Scrollback capture stores the tail for `provider == "daemon"` panes
  too — the "server replays scrollback" assumption is false after a reboot.

**Render/damage + wake-path impact:** the statusbar chip and relaunch overlay
are chrome (`Full` damage, existing paths). Daemon deltas already arrive as
pane-only damage (`Panes`). No new wake sources, no polling: kills are fired as
best-effort off-loop tasks; the fallback event rides the existing `PaneEvent`
mpsc + waker.

**SQLite:** no schema change, no `user_version` bump — pid travels over the
control API; scrollback/cwd/cmd reuse existing `group_tabs` columns.

## Risks / Trade-offs

- [Agents keep burning tokens/CPU after quit — intended but surprising] →
  exit message with the kept-session count, statusbar chip, and a
  Quit-and-kill action.
- [Daemon crash kills all panes] → same blast radius as today's UI crash,
  while a UI crash now _preserves_ panes (net win); supervision deferred.
- [Cold-daemon first-prompt latency (~100-150ms)] → measure with `just bench`;
  optionally fire a background `ensure_daemon` after first frame.
- [Residual session leaks (tab deleted while detached, DB reset)] → visible in
  `thegn session list`; smart GC deferred.
- [Behavior change for existing users] → `[daemon] enabled = false` restores
  the old lifecycle; config docs updated.

## Migration Plan

Config-only rollback (`enabled = false`, or a non-zero `lease_grace_secs`).
Existing DBs need no migration. Smoke's "no daemon by default" check is
reworded to assert CLI verbs never spawn a daemon (still true — only pane
spawns `ensure_daemon`).

## Open Questions

_None — defaults and scope confirmed with the user._
