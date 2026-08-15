# Known issues — 0.1.0-alpha.1

This is a public **alpha**. The items below are known, tracked, and deferred to
a later release — either because the fix carries regression risk not worth
taking right before the first release, or because they are narrow edge cases.
Full detail (evidence, file:line, proposed fix) lives in
[`docs/superpowers/specs/alpha-audit-2026-08.md`](docs/superpowers/specs/alpha-audit-2026-08.md).

If you hit one of these, it's a known limitation — but a reproducible report is
still welcome.

## Performance (event loop)

A handful of DB opens and one class of subprocess/network work run **on the
event loop** rather than off-thread. None block indefinitely (the DB uses a 5s
busy-timeout ceiling), and idle CPU stays ~0%, but under contention they can
cost a frame:

- Sandbox `ensure` / provider network calls can run on the loop via a
  `launch_spec` path in `pty_drain.rs` — a slow provider stalls the frame.
- A large paste into a non-reading / flow-controlled PTY does a blocking
  `write_all` on the loop (`pane.rs`).
- Window-title persistence, sidebar/panel `ui_state`, and the
  `persist_session_layout` heavyweight persist open the DB inline
  (`run.rs`, `handlers/sidebar_persist.rs`, `pty_drain.rs`).
- New-terminal wizard submit spawns the terminal synchronously on the loop,
  including a keyring read (`run.rs`, `onboarding.rs`).

Moving these off-thread is a mechanical but wide change to the ~18k-line loop;
deferred to keep the alpha's render invariants stable.

## Daemon / remote serving (`thegn serve`)

`thegn serve` (remote thin clients) is the newest and least-exercised surface.
Hardened this release (loopback default bind, owner-only socket), but still:

- No protocol/version-skew handshake between an old daemon and a new client —
  a mismatched pair can misbehave rather than refuse cleanly.
- `thegn serve` self-terminates after `idle_exit_secs` even with the TCP
  listener up but no sessions (kills the control plane out from under a client
  that hasn't attached yet).
- Disabling the daemon (`[daemon] enabled = false` / `THEGN_NO_DAEMON`) with
  persisted daemon panes can duplicate a pane on the fallback path.
- Reconnect / lag-resync replays up to 2000 scrollback lines as ordinary
  output, which can visually duplicate history on a flaky link.
- Control-plane git verbs run against a caller-supplied worktree path with no
  confinement to registered worktrees (unix socket is same-uid only, so this is
  a defense-in-depth gap, not a remote hole).

## Concurrency

- The reused merge-gate worktree has no **cross-process lock**: two concurrent
  `land`/`drain`/`integrate` runs (e.g. two terminals, or CI + a human) can gate
  the same worktree at once and interleave. Run one at a time for now.
- A few best-effort persists (focus/active-tab, corner-pane parsing) have
  benign unordered-writer races; last-writer-wins, no corruption.

## CLI / tracker

- Issue "assign to me" shows an optimistic success even if the tracker API call
  fails (the assignment may not have landed).
- Several worktree-targeting verbs mix a `--worktree` flag with a positional
  argument; this will be unified in a later release.

## Platform

- **Windows** support is best-effort: native panes only (no container
  sandboxing — that's a Linux/WSL2 feature), and requires a modern terminal
  (Windows Terminal; legacy conhost is refused).
- Cloud execution providers, remote worktrees over SSH, the Observe dashboards,
  the placement engine, and non-GitHub issue trackers are **dev-channel only**
  in this release (`THEGN_CHANNEL=dev`) and not considered stable.

## Config

- `thegn config validate` only type-checks a subset (~12 of ~50) of the
  enum-typed config keys, so an out-of-range value for an uncovered enum can
  pass validation (it is still caught — and rolled back — by `thegn config set`,
  which now re-parses the whole file after writing).
