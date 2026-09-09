# Tasks — make-daemon-default

## 1. Kill-on-close semantics

- [x] 1.1 Add `kill_session(&self, session: &str)` to the `ExecSource` trait
      (`crates/thegn-host/src/pane_source.rs`), default no-op; implement on
      `DaemonSource`/`LazyDaemonSource` (`crates/thegn-host/src/daemon/client.rs`)
      via `ControlClient::kill`.
- [x] 1.2 Add a shared `Arc<AtomicBool> detach_on_drop` (default false = kill) + `set_detach_on_drop()` to stream `PtyPane`; on `SessionEnd::PaneGone`
      with the flag false, fire a best-effort off-loop `kill_session(sid)`
      (`crates/thegn-host/src/pane.rs`).
- [x] 1.3 Create `crates/thegn-host/src/handlers/daemon_lifecycle.rs` with
      `mark_session_panes_detached(&session, &mut panes)` (center-tree panes
      only); call it from the `Action::Quit` arm in run.rs (2-line arm — the
      file is ratchet-pinned).
- [x] 1.4 Unit test: kill-vs-detach on drop with a hand-built `ExecSource`
      (pattern in `pane.rs` tests).

## 2. Ephemeral bypass

- [x] 2.1 Add `Panes::spawn_argv_env_local` (skips `daemon_cfg`) in
      `crates/thegn-host/src/panes.rs`; route pins (`spawn_pin`), tool-drawer
      spawns, and the corner overlay through it.
- [x] 2.2 Unit test in `panes.rs`: local spawn never routes through the daemon
      even with `daemon_cfg` installed.

## 3. Graceful attach-expiry fallback

- [x] 3.1 In `relay_exec` (`crates/thegn-host/src/pane.rs:785-814`): on initial
      `ExecOpen::Attach` failure, fall back to `source.open(&fallback)` before
      husking; emit `PaneEvent::SessionFallback(pane_id)`.
- [x] 3.2 Handle `SessionFallback` in `pty_drain.rs::drain` → call into
      `handlers/daemon_lifecycle.rs`: repaint the persisted scrollback tail and
      arm `set_pending_relaunch` from `tab.pane_cmds` (mirror `panes.rs:731-756`).
- [x] 3.3 Unit test: initial-attach failure produces a fallback open + event,
      not an exit husk.

## 4. Reboot fidelity

- [x] 4.1 Carry the daemon child pid in `SessionInfo`/attach `Hello`
      (`crates/thegn-svc/src/control/` types; filled in
      `daemon/service.rs::open`); set `PtyPane.pid` for daemon panes so
      `/proc`-based `capture_pane_cwds`/`capture_pane_cmds` work unchanged.
- [x] 4.2 Store the scrollback tail for `provider == "daemon"` panes in
      `capture_pane_scrollback` (`crates/thegn-host/src/snapshot.rs:100-122`).

## 5. Default flip

- [x] 5.1 `crates/thegn-core/src/config_daemon.rs`: `enabled: true`,
      `lease_grace_secs: 0` (= never reap); keep `idle_exit_secs = 1800`;
      update the defaults contract test (default-on / never-reap /
      idle-bounded); make the reaper treat `0` as infinite if it doesn't already.
- [x] 5.2 Update the `[daemon]` block in `config/config.toml.example`
      (~line 2409 — currently says "OPT-IN"); document `lease_grace_secs = 0`.
- [x] 5.3 `test/smoke.sh` (~493-570): reword the "no daemon by default" check
      to "CLI verbs never spawn a daemon"; optionally add a DELETE-kills-session
      check.

## 6. UX polish

- [x] 6.1 `push_daemon_chip` in `crates/thegn-host/src/statusbar_badges.rs`:
      "◆ persistent" chip (ASCII-degraded via `caps::active_glyphs()`) when the
      focused pane is daemon-backed.
- [x] 6.2 `Action::Detach` in `keymap.rs` + `ACTION_SPECS` (`keymap_specs.rs`),
      palette-visible, no default chord; palette-only "Quit and kill sessions"
      (best-effort off-loop kills, then quit). Dispatch arms → thin calls into
      `handlers/daemon_lifecycle.rs`.
- [x] 6.3 Exit message from `main.rs` after `run()` returns: "kept N sessions
      running — run `thegn` to reattach, `thegn session list` to inspect".

## 7. Docs, specs, verification

- [x] 7.1 tasks.md housekeeping: mark group A item 7 done (points at the
      control-plane spec), annotate items 111/120; item 8 stays open. Archive
      the completed `add-control-plane-and-remote` change.
- [x] 7.2 Manual verification with `just start name=dev`: run `top` → quit →
      `thegn session list` shows it live → relaunch → same screen. Reboot sim:
      kill the daemon pid → relaunch → fresh shell + scrollback tail + relaunch
      overlay (no husk). `just bench` first-frame delta; `THEGN_PERF=1` idle
      check.
- [x] 7.3 Run `just ci` (fmt + lint + build + test + openspec-validate +
      coverage + smoke + nix-build) once, when the implementation is complete.
