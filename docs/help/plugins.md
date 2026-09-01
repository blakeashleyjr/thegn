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
  `api = "0.2.0"`, `command = ["…"]`, contributions).
- A directory: `<config dir>/plugins/<name>/plugin.toml` with the same
  fields. Its `cwd` defaults to that directory.

Check what thegn sees with `thegn plugin list`; validate everything (api
compatibility, command presence, contribution acceptance) with
`thegn plugin check` — it exits non-zero on problems, so it fits a hook.

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
`admin` lattice as control-API tokens. Undeclared means denied, and every
denial is audited.

Crashed resident plugins restart with backoff (three attempts, then disabled
until config reload). Plugin processes are _not_ sandboxed — treat a plugin
like any program you choose to run.

## Writing one

The wire format and every verb live in the developer docs
(`docs/extending/plugin.md` and `openspec/specs/plugin-api` in the repo).
Start from the example, keep stdout pure JSON, and print one message per
line.
