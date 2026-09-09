# Design — debugger capability audit

## Observed contract

- One supported debugger: the managed BugStalker binary pinned by the tool
  catalog.
- One supported platform gate: Linux x86-64.
- Commands: setup/path plus foreground run and attach.
- Sessions inherit the pane/process environment; there is no independent remote
  debugger transport.
- Doctor is detection-only and reports resolution tier, installed-versus-pinned
  state, and the platform restriction.

## Decision

Do not retrofit a generic registry into the completed audit. A registry changes
trusted config, command templating, CLI, platform gating, doctor output, and
potentially the plugin/runtime security boundary. DAP additionally creates a
stateful protocol and UI ownership question. THE-102 must decide and specify
those surfaces before implementation.

## Non-goals

- `[[debug.adapters]]`
- lldb, delve, or editor adapter auto-discovery
- `--adapter` selection
- DAP client/server behavior
- plugin debugger providers or debug UI
