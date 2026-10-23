---
id: plugins
title: Plugins
order: 32
actions: []
---

# Plugins

Extend thegn with external programs speaking newline-delimited JSON — no
compile step, no special runtime: a shell script is a valid plugin. The
bundled example (`examples/plugins/hello.sh` in the repo) is a two-line
statusbar segment.

## Declaring a plugin

Two equivalent homes:

- A `[[plugins]]` entry in your config (the full spec: id, name, version,
  `api = "0.3.0"`, `command = ["…"]`, contributions).
- A directory: `<config dir>/plugins/<name>/plugin.toml` with the same
  fields. Its `cwd` defaults to that directory.

Plugins start with a clean environment: only PATH, HOME, the user, locale,
time-zone, terminal, and temp-directory variables (plus the Windows system
variables a process needs) are passed through, along with the plugin's own
`env` map. Tokens, agent sockets, and thegn's own variables are not passed
through unless the user's `[[plugins]]` entry sets `trusted_native = true`,
which keeps the full environment. In particular, untrusted plugins do not
receive `XDG_*` config, state, data, or runtime paths; `DISPLAY`,
`WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `SHELL`, `SSL_CERT_FILE`,
`NIX_SSL_CERT_FILE`, `RUST_LOG`, or proxy variables. Notification and GUI
plugins, plugins that need HTTPS certificate configuration on non-NixOS
systems, and plugins that call back into `thegn` need `trusted_native = true`
or explicit entries in their `env` map. Values may be
`keyring:NAME`, `env:VAR`, or `file:PATH` references, resolved just before the
plugin starts; other values pass verbatim. Only a `[[plugins]]` entry in your
config may use references: a reference in a directory `plugin.toml` refuses
that plugin. An empty or unresolved reference also refuses the plugin, with
the reason in the status line and log. Config output shows a reference as
written, never its resolved value, and masks literal values under
secret-looking names such as `*_TOKEN`.

Every plugin gets `THEGN_PLUGIN_STATE_DIR`, a private (owner-only) directory
under thegn's state directory (`plugins/<id>`), created before the plugin
starts; if it cannot be created safely (for example a symlink sits in its
place) that plugin is refused and the others still start. It is a place to
keep state, not a sandbox: every plugin runs as your user.

Check what thegn sees with `thegn plugin list`; validate everything (api
compatibility, command presence, contribution acceptance) with
`thegn plugin check` — it exits non-zero on problems, so it fits a hook.
For each enabled plugin, `check` prints the negotiated plugin/host API versions,
host-call scopes, granted and missing capabilities, and every accepted or
rejected contribution with its rejection reason. Inspection never starts the
plugin process.

`plugin list` is an offline inspection and does not attach to the compositor.
Its stable `state` vocabulary is `disabled-by-config`, `starting`, `healthy`,
`degraded`, `crash-disabled`, `stopped`, and `unknown`. A configured enabled
plugin is therefore reported as `unknown` with `live_health = unavailable`;
the command never infers health from a manifest or process absence. Failure,
restart-count, and backoff fields are `null` when no authoritative supervisor
snapshot is attached. Both text and `--json` retain exit-zero inspection
semantics; `plugin check` remains the validation command with failure exits.

Startup, reload, `thegn plugin list` and `thegn plugin check` use the same
admitted plugin set. `plugin list` therefore shows the same admit or refuse
decision and reason that `plugin check` reports, including set-wide problems
such as duplicate IDs, together with the source digest each plugin was
admitted with.

## Modes and rendering

- `mode = "one_shot"` (default): thegn runs the command on the cadence its
  contribution declares (`interval` milliseconds). It reads output until the
  plugin exits, then gives the pipes about half a second to drain; a background
  child cannot keep the run open by holding them. If it has not exited by its
  timeout, the run times out. Output still unread at the reader deadline is
  discarded; messages already read are kept. Each run accepts at most 20,000
  lines, 1 MiB per line, 8 MiB of retained stdout, and keeps up to 8 KiB of
  stderr.
  At most four one-shot runs, scheduled or started from the palette,
  run at once across all plugins. Each plugin's first one-shot run uses the
  same limit: if the cap is full, it stays pending until a later scheduler
  tick, and is dropped if the plugin is disabled meanwhile. When more plugins
  are due than the limit allows, the least recently started go first so every
  plugin gets a turn. A scheduled run waits for its next interval if that
  limit is reached or its previous run is still going; missed ticks are
  skipped and never replayed. A palette action reports `... is already running` or
  `... not started: too many plugin runs in flight (4 max)` in the status
  line. A run still in progress when its plugin is disabled, plugins are
  reloaded, or thegn quits finishes, but its output is discarded.
- `mode = "resident"`: one long-lived process for the session; thegn sends
  `activate`, then `render` per cadence, and `deactivate` on shutdown.

A `StatusBarSegment` contribution's `update` messages paint a segment in the
statusbar; a `NotificationSource`'s `notify` messages land in the
notification center; a `PaletteAction` contribution appears as a row in the
[[command-palette]] — invoking it sends the plugin an `on_event`
(`kind: Action`) if it is resident, or runs it once if it is one-shot. Lines that are not valid JSON are kept as diagnostics
(the most common mistake is a stray `echo`) — `thegn plugin check` and the
log surface them.

A view over 256 rows, 4096 spans or 128 KiB of text is rejected, and the
previous view stays in place. JSON frames nested more than 32 levels are
rejected before parsing.

Plugin declarations are bounded too: a manifest may have up to 64 contributions
and 64 capabilities. Each contribution allows 32 metadata entries (keys up to
256 B and values up to 1 KiB); labels and chords can be up to 256 B. The plugin
ID, name, and version, plus contribution, surface, and capability identifiers,
are limited to 128 B, and contribution capability data to 4 KiB. `thegn plugin
check` reports the specific quota reason when an oversized manifest is rejected.
Command arguments, `cwd`, and `env` keys and values may not contain NUL bytes;
`env` keys must match `[A-Za-z_][A-Za-z0-9_]*` and be at most 128 bytes, all
launch fields together are limited to 64 KiB, and plugin ids differing only by
letter case are duplicates.
An alert source over 128 B is refused; a message longer than 1 KiB is truncated
for display.
A plugin may subscribe to 64 event kinds; a 65th subscription is refused.
State, views, and retained events share an 8 MiB per-plugin limit. When state or a view needs
room, the oldest retained events are evicted first; a new item is refused if
state and views alone would exceed the limit. A new event is refused if it
cannot fit alongside state and views, otherwise older events are evicted as
needed. The 256-event queue also drops the oldest 64 events when full.

Resident plugin messages use a bounded queue of 1024 per plugin and 8192 total.
Repeated `update` and `invalidate` notifications for one surface, and `state.set`
for one key, coalesce to the latest value. At the limit, notifications are
dropped; requests get a `rate_limited` reply with `host plugin queue full`.
Saturation logs one `thegn::plugin` warning per plugin per flush. Exit is never
dropped.

Those three rendering/event points plus `IssueProvider` are the extension
points the general compositor host accepts. `PanelSection`, `SidebarTab`,
`Theme`, `Automation`, `HarnessAdapter`, `ProgramAdapter`, `CiProvider`,
`ForgeProvider`, and `DebugAdapter` are wire vocabulary only there:
`thegn plugin check` rejects them until their host runtime lands. `DataSource`
has a separate, calendar-account-specific command adapter; it is not a general
UI contribution. Native theme files and configured drawer tools are not runtime
plugin surfaces.

SidebarTab, Theme, and plugin key zones are adopted for a future release
(THE-107) but are not implemented, so `thegn plugin check` still rejects them.
Plugin themes will be data-only palettes, and plugin keys will be
surface-local actions.

The machine-readable source for this classification is embedded in
`docs/api/plugin-api-0.3.json` as `x-thegn-extension-support`; its companion
`x-thegn-host-verb-support` table names verb authority and permitted modes, and
`x-thegn-host-call-capabilities` is the exact set of dispatchable catalog ids.
`StatusBarSegment`, `NotificationSource`, and `PaletteAction` accept one-shot
or resident plugins. `IssueProvider` is resident-only because provider calls
need a reply channel. Palette actions and providers are on-demand; status and
notification contributions may also use an interval. Other cadence/mode
combinations fail `plugin check` with an actionable reason.

## Provider plugins

A plugin can _be_ a provider: an `IssueProvider` contribution makes the
plugin an issue-tracker backend — the host bridges the issue seam's
operations to it as `provider.call` requests, and its issues join the
panel beside your configured accounts (the contribution's label is the
account name). Its contribution `caps` object may declare `comments` and
`labels`; omitted or `null` caps are all false, and a false-cap optional call
is rejected locally without a `provider.call` round trip. The host uses the
existing issue JSON shapes and maps an RPC `unsupported` reply to the same
typed unsupported behavior. Unanswered calls time out at the plugin's
`timeout_secs` (clamped to 1–3600 seconds with a warning) and surface like any
provider error. Re-invoking a running one-shot reports that the contribution
is already running. `CiProvider`/`ForgeProvider`
are reserved wire vocabulary for the same pattern.

The standalone `thegn doctor` does not start resident plugins, but it lists
the native-trust state of each enabled plugin. Use `thegn plugin list` and
`thegn plugin check` to inspect and validate plugin declarations.

## Capabilities and host calls

The manifest's capabilities and the plugin's `scopes` gate what the host does for
it over the protocol; they do not contain the plugin process, which runs
unsandboxed as your user. Surfaces require their capability (e.g. `surface:statusbar`),
and `host.call` requests (invoking a host capability like `worktrees.list` by
catalog id) are checked against the plugin's `scopes` — the same `read`/`write`/`git`/
`merge_add`/`exec`/`admin` lattice as control-API tokens. `merge_add` is the
narrow, worktree-bound remote-enqueue grant. `write`, `git`, `merge_add`, and `exec` are
independent; `admin` implies all scopes. Undeclared means denied, and every
denial is audited. `tools.run` is the current exec-scoped plugin call;
`launch.preset` remains CLI-first and is not advertised to plugins until its
generic control route exists.

`host.call` has its own bounded dispatcher, separate from the resident-message
queue above: at most 64 calls are outstanding across all plugins, with at most
8 per plugin. Calls over either limit get `rate_limited`; a call that runs
longer than 30 seconds gets `timeout`, and a stopped dispatcher replies
`other`. A queued call is skipped if its plugin exits before the call runs. If
the plugin exits before its reply can be delivered, the reply is dropped and
counted as abandoned. Full or oversized reply frames are counted as
undelivered. Dispatcher pressure is warned once per plugin per drain.

A plugin is gated when it declares any non-surface capability or any scope.
An enabled gated plugin is refused unless its user's `[[plugins]]` entry sets
`trusted_native = true`. A directory plugin cannot trust itself: a
`trusted_native` value in its `plugin.toml` is ignored and reported as
`trusted_native in plugin.toml ignored`.
`thegn plugin list` shows `ungated`, `trusted-native`, or
`refused-untrusted` for each plugin's trust state. `thegn plugin check` prints
`native: trusted_native=<bool>; gated by [...]; <verdict>`. The
`Plugins (native code; capabilities are not a sandbox)` section in
`thegn doctor` lists each enabled plugin's id, source, trust state, and
`gated_by` capabilities; its JSON report includes the same rows.

Crashed resident plugins restart with backoff (three attempts, then disabled
until config reload). A pending restart is cancelled when the plugin is
disabled, on config reload and on shutdown; restarts wait on one background
timer rather than a thread each. That crash/backoff state is process-lifetime
supervisor state: it is not persisted across a daemon or compositor restart.
After such a restart, the offline `plugin list` command cannot reconcile live
state and reports an enabled plugin as `unknown` with
`live_health = unavailable` until a future attached-snapshot surface exists.
If the plugin host, restart worker, or a scheduled run cannot start, the status
line reports the failure. A restart that cannot be scheduled counts as a failed
restart, so the plugin is disabled after the usual attempts. A scheduled run
start failure is reported once until a run starts successfully.
Resident-plugin stderr is drained and, when debug logging is enabled, written
to the debug log at most 20 lines per second; a line is capped at 512 bytes,
and excess lines are suppressed. thegn keeps the last 8 KiB and line counts
internally; no command shows them yet.
Plugin processes are _not sandboxed_ — treat a plugin like any program you
choose to run. For a non-empty list, `thegn plugin list` ends with a note that
capabilities are not containment (its `--json` rows carry `"containment": "none"`),
and `thegn plugin check` prints `containment: none` for every plugin.

## Writing one

The wire format and every verb live in the developer docs
(`docs/extending/plugin.md` and `openspec/specs/plugin-api` in the repo).
Start from the example, keep stdout pure JSON, and print one message per
line.
