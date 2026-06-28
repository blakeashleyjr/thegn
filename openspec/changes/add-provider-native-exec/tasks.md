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

## 6. CLI-free control plane (Route A) — LANDED

The resident bridge intercepts every control-plane op at the `git::run`/`run_w`
seam once registered; the bridge agent is now started over native exec.

- [x] 6.1 `connect_native` in `bridge_sup.rs`: start `szhost bridge` via
      `provider.open_exec(tty=false)`; `connect_worktree_bridge` (`run.rs`) prefers
      it for an exec_api provider (`exec != cli`), else the CLI `bridge_command`.
- [x] 6.2 `ExecSession`→`Read`/`Write` adapter (`FramesReader`/`ControlWriter`,
      blocking off the runtime workers they run on) feeding `BridgeClient::new`;
      `drive_exec` made tty-aware (non-PTY 1-byte stream framing) + adapter test.
- [x] 6.3 `provision_provider_repo` routes the clone through the bridge when up,
      else the CLI fallback.
- [ ] 6.4 Live-verify on a real sprite with a musl bridge binary
      (`nix build .#szhost-musl`, `SUPERZEJ_BRIDGE_BINARY`).

## 8. Polish (Phase D) — LANDED

- [x] 8.1 `exec=auto` health-gated fallback: a per-provider failure cooldown
      (`agent::native_exec_report`/`native_exec_healthy`); after a connect/exec
      failure, `auto` shell panes + the native bridge back off to the CLI for the
      cooldown, then retry. `api` always tries native. Unit-tested.
- [x] 8.2 Bounded mid-session reconnect: `relay_session` returns a `SessionEnd`;
      `relay_exec` reattaches via `attach_exec` on a transient drop (replays
      scrollback), capped at `MAX_DEAD_RECONNECTS` no-progress attempts.
- [x] 8.3 Trace span on the relay task (`szhost::frame` / `native_pane`) for the
      live pass. (Full `szhost::perf` wake-source attribution deferred.)

## 7. Out of scope — agent pane in a provider env

The AI **agent** pane (`attach_agent_pane`, `run.rs`) is not routed through native
exec. It's coupled to an out-of-band ACP channel (a localhost TCP port, or a
bind-mounted unix socket under the bouncer) that the host connects to — neither
reaches an agent running _inside_ a remote sprite, independent of how the pane is
spawned. A CLI-free agent-in-provider needs its own ACP transport (e.g. ACP
multiplexed over the resident bridge, or a provider port-forward API) — a separate
design. Shell/split/new-tab/restore panes ARE covered (native exec + the Route A
control plane). Terminal panes (ssh/local) and the yazi drawer (local tool) are
intentionally left on their own paths.
