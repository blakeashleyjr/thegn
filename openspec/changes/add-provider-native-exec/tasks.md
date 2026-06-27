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
- [x] 5.2 `just _apply-backend`: relax the `sprite` CLI check to advisory.
- [ ] 5.3 Live verification on a real sprite (`SPRITES_TOKEN` + `sprite` CLI):
      no `sprite` process per pane, type/resize/exit, reattach replays scrollback.
