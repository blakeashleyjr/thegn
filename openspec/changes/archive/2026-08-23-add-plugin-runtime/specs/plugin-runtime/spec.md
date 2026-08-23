## ADDED Requirements

### Requirement: Plugins load from config and plugin directories

The host SHALL discover plugins from `[[plugins]]` config entries and from `<config_dir>/plugins/<dir>/plugin.toml` files (directory plugins defaulting `cwd` to their own directory), skip disabled entries, and validate each spec against the host contract (`HostContract::negotiate`): api compatibility, command presence, and contribution acceptance. `thegn plugin list` SHALL print the discovered set with mode/enabled/negotiation status, and `thegn plugin check` SHALL exit non-zero when any enabled spec fails validation, naming the problems.

#### Scenario: A directory plugin is discovered

- **WHEN** `<config_dir>/plugins/hello/plugin.toml` declares a valid v0.2 spec and the config declares none
- **THEN** the loader returns that spec with `cwd` = the plugin directory, and `thegn plugin list` shows it

#### Scenario: An incompatible api fails check

- **WHEN** a spec declares `api = "9.0.0"`
- **THEN** `thegn plugin check` reports the incompatibility and exits non-zero

### Requirement: Both plugin modes run without touching the idle loop

Resident plugins SHALL run as one long-lived process each, spoken to over NDJSON (`activate` on start, `render` per their cadence, `deactivate` on shutdown), with stdout parsed on a reader thread that forwards messages over a channel and pulses the terminal waker. One-shot plugins SHALL be executed on their `Interval` cadence by a scheduler thread through `spawn_ndjson`. No plugin work SHALL run on the event loop thread or before the first frame, and an idle session with plugins configured SHALL still make the loop block in `poll_input(None)`.

#### Scenario: Resident output wakes the loop

- **WHEN** a resident plugin writes an `update` message while the loop is blocked
- **THEN** the reader thread's channel send + waker pulse wake the loop and the handler applies the verb

#### Scenario: A crashed resident restarts with backoff

- **WHEN** a resident plugin exits unexpectedly
- **THEN** the host restarts it with capped backoff, and after the cap disables it until config reload, surfacing the state in `thegn plugin list`'s runtime column when attached

### Requirement: Verbs apply to the negotiated model

Incoming plugin messages SHALL be applied to the core `PluginRuntime`: `register` accepts only negotiated contributions; `update`/`invalidate` maintain the surface view cache; `notify` lands in the notification store (the NotificationSource surface); `state.get`/`state.set`/`host.value`/`subscribe`/`emit` behave per the plugin-api spec; junk lines are kept for diagnostics, never crash the host.

#### Scenario: A statusbar segment renders

- **WHEN** an accepted `StatusBarSegment` contribution's plugin sends `update` with a view for its surface
- **THEN** the statusbar renders that view via `draw_plugin_view` in the segment's slot on the next frame

### Requirement: host.call is scope-checked and dispatched

A `host.call` request SHALL be checked against the plugin's declared `scopes` using the capability catalog's `required_scope` before dispatch; a failing check answers `RpcError` with code `denied`. Granted calls for verbs the control client exposes SHALL be dispatched off-loop through the daemon control socket and answered with the result; verbs without a dispatch path answer code `unsupported`.

#### Scenario: An unscoped call is denied

- **WHEN** a plugin with `scopes = []` calls `{"cap": "sessions.list"}`
- **THEN** it receives an `RpcResponse` error with code `denied` and the audit log records the attempt

#### Scenario: A granted read call answers

- **WHEN** a plugin with the `read` scope calls `worktrees.list` while the daemon is up
- **THEN** it receives the worktree list as the `result`
