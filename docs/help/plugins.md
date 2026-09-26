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

## Modes and rendering

- `mode = "one_shot"` (default): thegn runs the command on the cadence its
  contribution declares (`interval` milliseconds) and reads messages until
  it exits.
- `mode = "resident"`: one long-lived process for the session; thegn sends
  `activate`, then `render` per cadence, and `deactivate` on shutdown.

A `StatusBarSegment` contribution's `update` messages paint a segment in the
statusbar; a `NotificationSource`'s `notify` messages land in the
notification center; a `PaletteAction` contribution appears as a row in the
[[command-palette]] — invoking it sends the plugin an `on_event`
(`kind: Action`) if it is resident, or runs it once if it is one-shot. Lines that are not valid JSON are kept as diagnostics
(the most common mistake is a stray `echo`) — `thegn plugin check` and the
log surface them.

Those three rendering/event points plus `IssueProvider` are the extension
points the general compositor host accepts. `PanelSection`, `SidebarTab`,
`Theme`, `Automation`, `HarnessAdapter`, `ProgramAdapter`, `CiProvider`, and
`ForgeProvider` are wire vocabulary only there: `thegn plugin check` rejects
them until their host runtime lands. `DataSource` has a separate,
calendar-account-specific command adapter; it is not a general UI
contribution. Native theme files and configured drawer tools are not runtime
plugin surfaces.

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
`timeout_secs` and surface like any provider error. `CiProvider`/`ForgeProvider`
are reserved wire vocabulary for the same pattern.

The standalone `thegn doctor` does not start resident plugins or inventory
live plugin providers. Use `thegn plugin list` and `thegn plugin check` to
inspect and validate plugin declarations.

## Capabilities and host calls

A plugin only gets what its manifest declares and the host grants: surfaces
require their capability (e.g. `surface:statusbar`), and `host.call`
requests (invoking a host capability like `worktrees.list` by catalog id)
are checked against the plugin's `scopes` — the same `read`/`write`/`git`/
`merge_add`/`exec`/`admin` lattice as control-API tokens. `merge_add` is the
narrow, worktree-bound remote-enqueue grant. `write`, `git`, `merge_add`, and `exec` are
independent; `admin` implies all scopes. Undeclared means denied, and every
denial is audited. `tools.run` is the current exec-scoped plugin call;
`launch.preset` remains CLI-first and is not advertised to plugins until its
generic control route exists.

Crashed resident plugins restart with backoff (three attempts, then disabled
until config reload). Plugin processes are _not_ sandboxed — treat a plugin
like any program you choose to run.

## Writing one

The wire format and every verb live in the developer docs
(`docs/extending/plugin.md` and `openspec/specs/plugin-api` in the repo).
Start from the example, keep stdout pure JSON, and print one message per
line.
