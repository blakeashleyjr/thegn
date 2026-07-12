# Make daemon-backed pane persistence the default

## Why

The pane daemon (control-plane spec) already delivers tmux-style persistence —
quit the UI, agents keep running, relaunch warm-reattaches — but it is opt-in
(`[daemon] enabled = false`) and has three correctness gaps that make default-on
a session-leak machine. Users expect "quit thegn, come back, start back up in
the same spot" to be the out-of-the-box behavior.

## What Changes

- **BREAKING (behavioral):** `[daemon] enabled` defaults to `true` — new local
  panes route through the pane daemon and survive quitting the compositor;
  bare `thegn` warm-reattaches to the same live screens.
- **BREAKING (behavioral):** `lease_grace_secs` defaults to `0` = never reap
  (tmux semantics: a detached session lives until explicitly killed or the
  machine reboots). `idle_exit_secs` stays 1800 (a daemon with zero sessions
  still exits).
- Explicitly closing a pane/tab **kills** its daemon session instead of leaking
  a lease (new `ExecSource::kill_session`, wired through drop semantics with a
  quit-time detach override).
- Ephemeral panes — pins, tool drawer, corner overlay — bypass the daemon
  (in-process PTYs); only center-tree worktree panes persist.
- An initial reattach failure (expired/reaped session, post-reboot fresh
  daemon) degrades to a **fresh shell + persisted scrollback tail + relaunch
  overlay** instead of an error husk.
- Reboot fidelity: the daemon reports its child pid so cwd/foreground-cmd
  capture works for daemon panes, and scrollback snapshots are persisted for
  them too.
- UX: statusbar "persistent" chip on daemon-backed panes, palette actions
  **Detach** (quit, keep panes) and **Quit and kill sessions**, and an exit
  message ("kept N sessions running — run `thegn` to reattach").

## Capabilities

### New Capabilities

_None._

### Modified Capabilities

- `control-plane`: Headless-daemon requirement gains default-on routing with
  explicit-close-kills and ephemeral-bypass semantics; warm-reattach
  requirement gains the graceful expiry-fallback (fresh shell + scrollback
  tail + relaunch overlay, never an error husk) and reboot-fidelity (pid,
  scrollback capture); persistent-relay requirement changes lease default to
  never-reap with `lease_grace_secs = 0` meaning infinite.

## Impact

- **Roadmap:** tasks.md group A item 7 (headless daemon — completes), group I
  items 111 (detach/attach — polish) and 120 (background keep-alive — the
  never-reap lease is the keep-alive), item 8 (daemon supervision) stays open.
- **Code:** `crates/thegn-host/src/{pane.rs,panes.rs,pane_source.rs,snapshot.rs,main.rs}`,
  `crates/thegn-host/src/daemon/{client.rs,service.rs}`,
  `crates/thegn-svc/src/control/` (SessionInfo/Hello pid field),
  `crates/thegn-core/src/config_daemon.rs` (defaults + contract test), new
  `crates/thegn-host/src/handlers/daemon_lifecycle.rs` (god-file ratchet:
  run.rs arms stay thin calls), `statusbar_badges.rs`, `keymap.rs`/`keymap_specs.rs`.
- **Config/docs:** `config/config.toml.example` `[daemon]` block (currently
  says "OPT-IN"), `test/smoke.sh` daemon-section labels.
- **No AI-layer dependency:** all changes live in the AI-free shell.
