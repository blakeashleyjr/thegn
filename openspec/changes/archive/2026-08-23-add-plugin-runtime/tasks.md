## 1. svc: loader + session

- [x] 1.1 `plugin/loader.rs`: `discover(cfg, config_dir)`, `check_spec` (negotiate + command presence), unit tests (dir plugin, disabled skip, bad api)
- [x] 1.2 `plugin/session.rs`: `ResidentSession` (spawn, reader thread → channel, writer handle, kill), env scrub, unit tests with a shell child

## 2. host: runtime + handlers + statusbar

- [x] 2.1 `plugins.rs`: `PluginsHost` (specs, runtimes, sessions, scheduler thread, backoff restart), started off-thread post-first-frame with channel + waker
- [x] 2.2 `handlers/plugins.rs`: drain + apply verbs to `PluginRuntime`; notify → NotificationStore; host.call scope check + dispatcher thread over the control client
- [x] 2.3 Statusbar: render accepted StatusBarSegment views via `draw_plugin_view`; run.rs wiring (start, drain, shutdown deactivate)
- [x] 2.4 Config hot-reload restarts the runtime

## 3. CLI + example + docs

- [x] 3.1 `cmd/plugin.rs`: `thegn plugin list|check` (offline)
- [x] 3.2 `examples/plugins/hello.sh` + golden test through loader/apply
- [x] 3.3 `docs/help/plugins.md` (registered page) + `docs/extending/plugin.md` refresh; SURFACE_GAPS labels updated (dispatched verbs burn)

## 4. Gate

- [x] 4.1 clippy + suites (svc plugin, host plugins/handlers, example golden) + `just lint`; openspec validate
