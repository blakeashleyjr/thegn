# Tasks — native Windows phase 1 (compile)

## 1. Core seams

- [x] 1.1 `thegn_core::shellinv` — `ShellFlavor`/`flavor_of`/`run_argv`/
      `exec_argv` (pure; unit tests cover posix/pwsh/cmd dialects).
- [x] 1.2 Fix the never-compiled `cfg(windows)` `util::shell()` branch
      (`which_path` returns `Option`, not `Result`).
- [x] 1.3 `util::detached()` — gate `process_group(0)` to unix; windows uses
      `CREATE_NO_WINDOW`.
- [x] 1.4 `startup.rs` gitconfig repair — cross-platform `symlink_file` helper.
- [x] 1.5 `sandbox.rs` Win backend argv arm delegates to `shellinv::run_argv`
      (pinned file shrinks).

## 2. Service layer gating

- [x] 2.1 `control/client.rs` — `ControlAddr::Unix` arms + `WsEither::Unix`
      gated; Windows returns "daemon IPC is not yet supported on Windows".
- [x] 2.2 `acp/transport.rs` — `connect_unix` gated with an explicit Windows
      error stub.

## 3. Host platform seam

- [x] 3.1 `platform/mod.rs` + `unix.rs` + `windows.rs`: `StderrGuard`/
      `redirect_stderr_to_logfile`, `pid_alive`, `terminate_pid`,
      `set_process_group`, `kill_tree`, `install_shutdown_signal`,
      `spawn_shutdown_notifier`.
- [x] 3.2 Rewire run.rs (stderr guard, shutdown signal — pinned file shrinks),
      daemon/mod.rs (notifier, pid_alive), share.rs, proxy_daemon.rs,
      task.rs, merge_driver.rs (groups/kills through the seam).
- [x] 3.3 `frame_write.rs` transient-IO classification: ErrorKind arm shared;
      raw-errno tail unix-only; EIO-based tests gated `cfg(unix)`.
- [x] 3.4 `profile.rs` SIGUSR2 profiler gated `all(feature = "profiling", unix)`.
- [x] 3.5 `vps_bridge.rs`/`cmd/debug.rs` exec-replace: unix `exec()`, windows
      spawn+wait+exit (vps) / explicit error (debug).
- [x] 3.6 relay.rs (sealed-sandbox model relay) unix-gated with an
      `Unsupported` stub; daemon `run()` unix-gated with a clear bail.
- [x] 3.7 Ungated chmod/symlink sites gated (`agent.rs` proxy wrapper,
      `agent_configs.rs` test).
- [x] 3.8 Local shell spawns through `shellinv` (pins argv, tool drawer,
      run.rs custom action / pane-run / editor-open).
- [x] 3.9 Remote/sandbox `sh -lc` sites annotated
      (`agent_pi.rs`, `hibernator.rs`, `agent.rs` ExecSpec).

## 4. Dependencies & build

- [x] 4.1 thegn-host: `nix`/`libc` → `[target.'cfg(unix)'.dependencies]`;
      `windows-sys` (Foundation, Console, Threading, JobObjects) added under
      `cfg(windows)`; workspace entry in root Cargo.toml.
- [x] 4.2 `just check-cross` gains `cargo check --workspace --target
      x86_64-pc-windows-gnu`; dev shell provides the mingw-w64 cross cc
      (`CC_x86_64_pc_windows_gnu` etc. in devenv.nix).
- [x] 4.3 ci.yml: opt-in `windows` job (windows-latest, rustup + bare
      `cargo check --workspace --locked`, `[ci-windows]` marker/dispatch).

## 5. Validation

- [x] 5.1 `cargo check --workspace --target x86_64-pc-windows-gnu` green
      locally/CI (Linux).
- [ ] 5.2 One `[ci-windows]` dispatch run green on windows-latest (msvc).
- [ ] 5.3 `just ci` green (pre-PR gate, run once at the end).
- [ ] 5.4 Waker spike on a real Windows box: termwiz blocking
      `poll_input(None)` + off-thread `TerminalWaker` wake (de-risks the
      compositor-validation change; scratch binary, not committed).
