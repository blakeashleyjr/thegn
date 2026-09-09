# Define the debugger adapter and DAP surface

Linear: THE-102 (Active, Plugin UI Platform)

## Why

thegn's supported debugger is a bounded BugStalker CLI handoff on Linux x86-64.
There is no generic adapter registry, DAP client/server, provider lifecycle, or
debugger UI contribution. Adding any one of those changes trusted process
execution and UI ownership and therefore requires an explicit architecture
decision before implementation.

## What Changes

- Decide independently whether to support launch adapters, DAP sessions, and
  plugin-contributed debugger UI.
- Publish a support matrix and provider boundary rather than treating these as
  one feature.
- If launch adapters are adopted, specify trusted config, argv templates,
  platform gates, selection, doctor, and backward-compatible BugStalker default.
- If DAP/UI is adopted, specify session ownership, protocol version/lifecycle,
  capabilities, budgets, input/render ownership, and plugin permissions.

## Impact

- Specs: new `debugger-extensibility` decision/adoption contract.
- Project: Plugin UI Platform.
- Existing `debug setup|path|run|attach` remains unchanged until a separately
  accepted implementation phase lands.
- Origin: implementation deliberately separated from THE-26's completed audit.
