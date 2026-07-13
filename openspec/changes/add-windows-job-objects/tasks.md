# Tasks — native Windows phase 3 (Job Objects)

## 1. Platform seam

- [x] 1.1 `GroupHandle` (Clone; pgid on unix, `Arc`-owned kill-on-close Job
      Object on Windows; `from_pid` degraded/test constructor) +
      `spawn_grouped` replacing `set_process_group`/`kill_tree`.
- [x] 1.2 Windows: `CreateJobObjectW` + `KILL_ON_JOB_CLOSE` +
      `AssignProcessToJobObject`; failed assignment degrades to direct-child
      termination instead of failing the spawn.
- [x] 1.3 `cfg(windows)` tests: `terminate()` reaps the `cmd /C ping` tree;
      dropping the last handle reaps it too (KILL_ON_JOB_CLOSE).

## 2. Call sites

- [x] 2.1 `task.rs`: registry stores `(generation, GroupHandle)`;
      cancel_slot/supersede/watchdog terminate through the handle; spawn via
      `spawn_grouped`. Existing cancel/timeout tests stay green.
- [x] 2.2 `merge_driver.rs`: agent watchdog holds a *clone* (the spawner's
      handle must outlive the child — kill-on-close) and terminates the job
      on deadline.
- [x] 2.3 Pane PTY children: not wired into jobs (portable-pty already scopes
      ConPTY); revisit only if manual testing shows orphans.

## 3. Warning-free windows target (drive-by parity)

- [x] 3.1 `profile.rs` singleton lock cross-platform (std `File::try_lock`);
      Windows stubs removed — `instance_running` now real on Windows.
- [x] 3.2 Stale cfg gaps tightened (`merge_driver` imports/helpers,
      `desktop_notify` import, `GitLock` RAII field note) —
      `cargo check --workspace --target x86_64-pc-windows-gnu` is warning-free.

## 4. Validation

- [x] 4.1 Linux: task/merge_driver/profile test modules green; clippy clean.
- [x] 4.2 windows-gnu workspace cross-check green (no warnings).
- [ ] 4.3 Windows CI (`[ci-windows]`): `cargo test -p thegn-host platform::`
      green on a real kernel.
