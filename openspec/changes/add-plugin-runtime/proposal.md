## Why

The plugin API v0.2 contract landed (wire types, `PluginSpec`, `PluginRuntime` model, schema snapshot) but nothing _runs_ a plugin: `spawn_ndjson` is only used by the calendar `command` source, `draw_plugin_view` is unwired, and `[[plugins]]` config is dead weight. The capability catalog pins this debt ("plugin host.call lands in the plugin-runtime phase"). Without a runtime, the extension story is vapor.

## What Changes

- **Loader** (`thegn_svc::plugin::loader`): merge `[[plugins]]` config entries with `<config_dir>/plugins/*/plugin.toml` directories (dir plugins default `cwd` to their directory); `check_spec` validates api compatibility (`HostContract::negotiate`), command presence, and contribution acceptance.
- **Resident session** (`thegn_svc::plugin::session`): one long-lived child per `mode = "resident"` plugin — NDJSON over stdin/stdout, reader thread forwarding parsed `RpcMessage`/`RpcResponse` lines plus an exit notice to a channel; writer handle for callbacks (`activate`/`render`/`on_event`/`deactivate`) and `RpcResponse` replies; same env scrub as `spawn_ndjson`.
- **Host runtime** (`thegn-host/src/plugins.rs` + `handlers/plugins.rs`): started off-thread after first frame (0%-idle: channel + waker pulse). One-shot plugins run on their `Interval` cadence via a scheduler thread through `spawn_ndjson`; resident plugins get `render` on cadence. Incoming verbs apply to the core `PluginRuntime`: `update`/`invalidate` fill the statusbar segment views, `notify` lands in the notification store (NotificationSource), `state.*`/`host.value`/`subscribe`/`emit` per the model. `host.call` is scope-checked (`PluginSpec::scopes` vs `required_scope`) and dispatched on a background thread through the control client (daemon socket) for the verbs it exposes (`sessions.list`, `worktrees.list`, …); everything else answers `RpcError::Unsupported`.
- **Statusbar**: accepted `StatusBarSegment` contributions render their cached views via the existing `draw_plugin_view`.
- **Lifecycle**: crashed resident sessions auto-restart with capped backoff; config hot-reload restarts the runtime. (No CLI `restart` verb — the DB-mailbox/control plumbing for one belongs to the client-API phase.)
- **CLI**: `thegn plugin list` (discovered specs, mode, enabled, negotiation status) and `thegn plugin check` (validation issues, non-zero exit on failure), both offline.
- **Example + golden test**: `examples/plugins/hello.sh` (a shell one-shot statusbar segment) exercised by a host test through the loader + apply path.
- **Docs**: `docs/help/plugins.md` (F1 page) + `docs/extending/plugin.md` refresh; `[[plugins]]` stays allowlisted in the config-example gate as a developer surface, documented in the help page.
- SURFACE_GAPS: plugin `host.call` entries for dispatched verbs burn; the rest re-labelled "generic dispatch lands in the client-API phase".

## Impact

- Audit row C1. Specs: new `plugin-runtime` capability; `plugin-api` delta (runtime honours the wire contract).
- Code: thegn-svc (`plugin/{loader,session}.rs`), thegn-host (`plugins.rs`, `handlers/plugins.rs`, `cmd/plugin.rs`, chrome/statusbar + run.rs wiring), thegn-core (capability gap labels), examples/, docs/.
