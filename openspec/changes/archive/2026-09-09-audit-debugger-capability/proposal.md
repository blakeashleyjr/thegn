# Audit debugger capability

Linear: THE-26 (Done)

## Why

The debugger surface needed a truth audit before expanding it. The audit found
a bounded, working BugStalker integration and an accurate base debugger spec,
but no generic adapter registry, DAP protocol, provider seam, or plugin surface.

## What Changed

- Confirmed the supported surface: `thegn debug setup|path|run|attach` using the
  pinned BugStalker tool on Linux x86-64.
- Confirmed doctor reports the managed tool's resolution/pin/platform truth and
  never starts a debugger.
- Removed draft claims for `[[debug.adapters]]`, lldb/delve, and `--adapter`.
- Recorded the architecture decision that generic adapters/DAP require a new
  provider decision and contract, now tracked by THE-102
  (`define-debugger-adapter-surface`).

## Impact

- Specs: one debugger doctor requirement; existing BugStalker requirements stay
  authoritative.
- No code or config claim for generic adapters.
- Follow-up: THE-102 owns any adapter/DAP implementation and Plugin UI surface.

## Archive status

THE-26 was an audit/decision ticket. Its bounded result is complete and
archive-ready; adapter implementation is intentionally not part of it.
