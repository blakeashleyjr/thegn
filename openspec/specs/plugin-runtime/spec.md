# plugin-runtime Specification

## Purpose

How thegn _runs_ plugins: discovery (`[[plugins]]` + `plugins/*/plugin.toml`), validation (`thegn plugin list|check`), process lifecycle for both modes (one-shot on cadence via `spawn_ndjson`; resident NDJSON sessions with activate/render/deactivate, crash backoff, hot-reload restart), verb application to the core `PluginRuntime` (statusbar segments, notifications, state), and scope-checked `host.call` dispatch through the daemon control socket. The wire contract itself is `plugin-api`; this spec is the runtime that honours it without ever touching the idle loop.

## Requirements

### Requirement: Plugins load from config and plugin directories

The host SHALL discover plugins from `[[plugins]]` config entries and from `<config_dir>/plugins/<dir>/plugin.toml` files (directory plugins defaulting `cwd` to their own directory), skip disabled entries, and validate each spec against the host contract (`HostContract::negotiate`): api compatibility, command presence, and contribution acceptance. `thegn plugin list` SHALL print the discovered set with mode/enabled/negotiation status, and `thegn plugin check` SHALL exit non-zero when any enabled spec fails validation, naming the problems.

#### Scenario: A directory plugin is discovered

- **WHEN** `<config_dir>/plugins/hello/plugin.toml` declares a valid v0.3 spec and the config declares none
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

A `host.call` request SHALL be checked against the plugin's declared `scopes`
using the capability catalog's `required_scope` before dispatch; a failing
check answers `RpcError` with code `denied`. The dispatchable set SHALL be
derived from the catalog — every row listing `Surface::Plugin` except
streaming rows — and dispatched generically through the same
capability-to-route spine `thegn api call` uses, off-loop over the daemon
control socket, so a newly routed catalog verb is callable by plugins with no
per-verb dispatch code. Capabilities outside the derived set answer code
`unsupported`; admin-scoped capabilities MUST remain unreachable by
construction (no admin row lists the plugin surface).

#### Scenario: An unscoped call is denied

- **WHEN** a plugin with `scopes = []` calls `{"cap": "sessions.list"}`
- **THEN** it receives an `RpcResponse` error with code `denied` and the
  audit log records the attempt

#### Scenario: A granted read call answers

- **WHEN** a plugin with the `read` scope calls `worktrees.list` while the
  daemon is up
- **THEN** it receives the worktree list as the `result`

#### Scenario: A newly routed verb needs no plugin-runtime change

- **WHEN** a catalog row listing `Surface::Plugin` gains its control route
- **THEN** a plugin with the required scope can `host.call` it immediately,
  with no new dispatch arm

#### Scenario: A granted git call performs the verb

- **WHEN** a plugin with the `git` scope calls `merge.add` for a worktree
- **THEN** the branch is enqueued exactly as via the HTTP surface, and a
  plugin holding only `read` receives `denied`

### Requirement: Palette actions route to their owning plugin

Accepted `PaletteAction` contributions SHALL appear as command-palette rows keyed `plugin:<plugin>:<contribution>`, listed from loop-owned plugin state (never from config-only palette construction). Invoking a row SHALL send a resident plugin an `on_event` notification with `kind: Action` and the contribution id as `payload.id`, or run a one-shot plugin once off-loop; a disabled plugin's rows SHALL be absent and its invocation refused with a status message.

#### Scenario: A resident action fires an event

- **WHEN** the user picks a resident plugin's palette row
- **THEN** the plugin receives `on_event` with `kind: Action` and `payload.id` naming the contribution

#### Scenario: A one-shot action runs the plugin

- **WHEN** the user picks a one-shot plugin's palette row
- **THEN** the plugin's command runs once off-loop and its messages apply exactly like a scheduled run

### Requirement: A plugin can be an issue provider

A resident plugin with an accepted `IssueProvider` contribution SHALL be bridged onto the issue seam: each `IssueBackend` operation is sent as a `provider.call` request (`{"seam":"issues","op":…,"args":…}`) and the plugin's `RpcResponse` is the operation's result, with `unsupported` errors mapping to the seam's optional-op fall-through and unanswered calls timing out at the plugin's `timeout_secs`. Live plugin providers SHALL join every `IssueRouter` the host builds, labeled by the contribution's label, and leave it on exit/disable. `CiProvider` and `ForgeProvider` SHALL be accepted wire vocabulary negotiated unsupported until their seams support dynamic selection.

#### Scenario: Plugin issues join the panel feed

- **WHEN** a resident plugin with an `IssueProvider` contribution answers `list_issues`
- **THEN** its issues merge into the router's results beside configured accounts, with provider slug `plugin:<id>`

#### Scenario: A silent plugin degrades, never hangs

- **WHEN** a bridged operation gets no reply within the plugin's timeout
- **THEN** the call returns a classified transport error and a late reply is dropped

### Requirement: Plugin issue capabilities are declared and enforced locally

An accepted `IssueProvider` contribution MAY carry the issue `caps` object.
Omitted or null caps SHALL mean all false, and unknown cap fields SHALL be
rejected. The host SHALL pass the declaration to `PluginIssueBackend`.
Operations behind a false cap SHALL return typed `Unsupported` without a
`provider.call` round trip. Operations behind a true cap SHALL use the existing
`provider.call` wire (`seam = "issues"`, operation name, serialized args), and
an upstream `unsupported` reply SHALL map to typed `Unsupported` as a second
degradation boundary. The existing five core operations, router composition,
timeouts, and old manifests remain compatible.

#### Scenario: An omitted optional capability fails locally

- **WHEN** an accepted `IssueProvider` omits `caps` and the host requests an optional comment operation
- **THEN** the bridge returns typed `Unsupported` without sending a `provider.call`

#### Scenario: A declared capability forwards through the bridge

- **WHEN** an accepted `IssueProvider` declares `caps.comments = true` and the host requests a comment operation
- **THEN** the bridge sends the existing issues `provider.call` and maps an upstream `unsupported` reply back to typed `Unsupported`

### Requirement: Plugin tracker caps share native conformance

The offline issue conformance suite SHALL cover a plugin bridge with omitted,
false, and true capability declarations using a scripted fixture. It SHALL
prove false-cap local refusal and true-cap forwarding without making a network
request. Standalone `thegn doctor` is not required to start resident plugins;
plugin manifest inventory is a follow-up.

#### Scenario: Plugin capability conformance stays offline

- **WHEN** the conformance suite exercises omitted, false, and true plugin issue capabilities
- **THEN** a scripted fixture proves local refusal and forwarding without network access

### Requirement: A resident plugin can subscribe to the control event feed

A resident plugin that declares an event-feed subscription SHALL receive
control feed events (activity, lease, session-list, exit, pairing) as
`on_event` notifications, gated by the `read` scope, delivered off-loop
through the plugin runtime's existing channel + waker path; pane byte streams
are never delivered this way. An undeclared or under-scoped plugin receives
nothing.

#### Scenario: A subscribed plugin sees an agent transition

- **WHEN** a `read`-scoped resident plugin has declared a feed subscription
  and a session's agent state changes
- **THEN** the plugin receives an `on_event` notification carrying the
  activity event, without any polling and without waking the idle render
  loop for a non-subscriber

### Requirement: Runtime negotiation exposes only wired support

The general plugin host SHALL advertise and accept only extension points whose
canonical support state is wired in that runtime. Separately implemented
adapter surfaces SHALL name their owning subsystem, and reserved points SHALL
decode but be rejected before activation. `plugin list` and `plugin check`
SHALL report the same classification without starting resident plugins.

#### Scenario: Manifest mixes wired and reserved contributions

- **WHEN** a manifest requests a wired statusbar segment and reserved panel
  section
- **THEN** negotiation reports each contribution's real state and does not
  silently activate the reserved section

### Requirement: Contract inspection is side-effect free

Version/support/scope inspection and `plugin check` SHALL validate manifests,
commands, and contribution negotiation without starting a resident plugin or
granting a scope not present in trusted configuration.

#### Scenario: Operator checks an exec-capable plugin

- **WHEN** `thegn plugin check` inspects a manifest requesting `exec`
- **THEN** it reports the requested/granted result but does not execute the
  plugin or any host capability
