# Design — target-first cold profile migration

## Boundary

The short-lived CLI process resolves both profile roots and opens their stores;
long-running profile daemons are not granted cross-profile reach. The target
must already exist. No target reroot or target config load occurs.

The migration allowlist is explicit: selected `tab_groups`/`group_tabs`, an
absent target worktree registration, exact group-scoped collapse/pin/ordinal UI
keys, path-scoped agent dispatches and notes, and only the active source
session's running-pin payload. Pane/session daemon IDs and dispatch session IDs
are cleared. Whole-session focus, attention, queues, accounts, pairings,
credentials, config, identity, and unrelated rows are excluded.

Opaque command, scrollback, report, artifact, and note payloads intentionally
cross because they are resurrection value. The command warns about them but
never serializes them into audit output.

## Liveness and consent

A running source compositor can race persistence, so any source profile
instance holding the singleton guard causes a preflight refusal. The command
also unions worktree-owned daemon sessions, IDs referenced by panes, and
dispatch session IDs. Without `--kill` confirmed live IDs block. With it, the
source control client kills each and a second listing must prove no survivor.
An unreachable registered daemon fails closed when liveness cannot be disproved.

## Transactions and recovery

Preflight checks every target group/UI/pin collision. One target transaction
inserts the transfer set, preserving target-owned worktree metadata. A stable
fingerprint over sanitized rows is read back before one source transaction
deletes the exact selected rows. This ordering chooses a visible duplicate
window over data loss. Retry recognizes an identical prior import; differing
content aborts.

Dry-run uses read-only store access and may not create/migrate a target DB or
sidecar. Completion output records transaction/fingerprint/cleanup state and
per-table counts. Target notification happens only after confirmed cleanup and
cannot roll back the move.
