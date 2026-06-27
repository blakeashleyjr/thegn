# Tasks

## 1. Provider seam (superzej-svc)

- [x] 1.1 `ExecSpec`/`ExecFrame`/`ExecControl`/`ExecSession` (channel handle, no
      `dyn` async trait); `Provider::open_exec`/`attach_exec` match-dispatch;
      `exec_api_by_name`; flip `caps().exec_api` for Sprites.
- [x] 1.2 Sprites WSS driver (`open_exec`/`attach_exec` → `start_session` →
      `drive_exec`): bearer auth, PTY binary frames, resize/exit/session JSON
      control frames; `tokio-tungstenite` (rustls) dep.
- [x] 1.3 Pure helpers + unit tests: ws-url derivation, exec query-string,
      resize JSON, control-frame classification.

## 2. Stream-backed pane (host)

- [x] 2.1 `PaneIo` enum inside the pane (Pty | Stream); `spawn_stream` +
      `relay_exec`/`relay_session` (bridge `ExecSession` ⇄ `PaneEvent` + waker);
      `write_input`/`resize` route per transport.
- [x] 2.2 `Panes::spawn_native`/`spawn_native_shell` + stored runtime handle.
- [x] 2.3 Unit test: a fake `ExecSession` drives the relay (output → grid,
      input/resize forwarded, session id published, exit).

## 3. Config + spawn decision (core + host)

- [x] 3.1 `ProviderExecMode { auto, api, cli }` (default auto) +
      `EnvProviderConfig.exec`; core parse/default test.
- [x] 3.2 `agent::native_shell_exec` (resolve env exactly as `launch_spec`) +
      `NativeShell::open_spec`; branch in `spawn_worktree_shell_pane`.

## 4. Session reattach (host + core)

- [x] 4.1 DB v22: `group_tabs.pane_sessions` column + migration; `GroupTabRow`
      field; `put`/`select`.
- [x] 4.2 `ProviderSession` + `Tab.pane_sessions` (persist/restore/remap/prune);
      `capture_pane_sessions`; `pane.provider_session()`.
- [x] 4.3 `materialize_with_specs` attaches the persisted session (else fresh
      native, else CLI).

## 5. Docs + launcher

- [x] 5.1 `config.toml.example`: document `exec`, drop the WSS-follow-up caveat.
- [x] 5.2 `just _apply-backend`: relax the `sprite` CLI check to advisory; scaffold
      API lifecycle (`auto_provision`/`auto_checkpoint` true, drop `up_command`/
      `down_command`) so sprite creation is also CLI-free.
- [ ] 5.3 Live verification on a real sprite (`SPRITES_TOKEN`): no `sprite`
      process per pane, type/resize/exit, reattach replays scrollback.

## 6. Follow-up — CLI-free control plane (Route A, separate change)

The interactive pane (exec) and lifecycle (create/checkpoint) are now CLI-free.
The chrome's control plane (git status/diff/log, gh, repo clone-on-open) still
shells `sprite exec` via `GitLoc::Provider`'s `control_prefix`. The resident
bridge ALREADY intercepts every control-plane op at the `git::run`/`run_w` seam
(`superzej-svc/src/git/mod.rs:455-507`) once a `BridgeClient` is registered — so
the only remaining CLI use is **starting** the bridge agent.

- [ ] 6.1 `bridge_sup.rs::bridge_command` (`:177-195`): for a `Placement::Provider`
      with `caps().exec_api`, start `szhost bridge` via `provider.open_exec(tty=false)`
      instead of the `control_prefix` CLI.
- [ ] 6.2 Adapt the async `ExecSession` (frames/control channels) to the blocking
      `Read`+`Write` that `BridgeClient::build` consumes (the reader runs on a
      std::thread; mind tokio `blocking_send`/`blocking_recv` context rules).
- [ ] 6.3 Route `provision_provider_repo` (`agent.rs:818`) through the bridge too,
      so clone-on-open is CLI-free; keep the CLI path as graceful fallback.
- [ ] 6.4 Requires a musl bridge binary (`nix build .#szhost-musl`,
      `SUPERZEJ_BRIDGE_BINARY`) pushed via the existing `ensure_executable` fs API;
      live-verify on a real sprite.
