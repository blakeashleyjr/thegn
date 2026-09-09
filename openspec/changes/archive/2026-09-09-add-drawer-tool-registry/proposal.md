# Drawer tool registry — arbitrary per-worktree and global drawer occupants

Linear: THE-11

## Why

The bottom drawer already provides a pooled, off-loop, contained PTY chrome
surface, but only the built-in files provider can occupy it. Users need
worktree tools such as ATAC as well as process-local global tools such as a
scratch database shell, with a discoverable indicator.

## Accepted change

Extend `[[tools]]` with optional `drawer_scope` (`worktree` or `global`) and
`drawer_cwd`. The existing command and environment fields remain authoritative.
The effective registry is the built-in files occupant followed by eligible
tools in config order. No second `[[drawer.tools]]` table or inline command
escape hatch is introduced.

The drawer displays one occupant at a time. Existing files-drawer toggling,
the new `drawer-cycle` action, and a dedicated `drawer-pick` palette share one
runtime transition boundary. Worktree occupants have per-worktree state and
panes. Global occupants have one process-local pane and global state slot,
reused across worktree switches. Pool limits, containment, cold resolution,
exit cleanup, and prewarm all remain within that runtime.

A removable `drawer` statusbar widget is enabled by default, names the active
occupant, shows the configured count, and toggles the drawer on click.

## Non-goals

- repo-local arbitrary command registries or new trust/security surfaces;
- daemon persistence or restart survival for global panes;
- SQLite migrations, CLI/control/MCP/plugin capability additions;
- multiple visible drawer panes, lifecycle hooks, or file-manager internals;
- re-recording the deferred chrome snapshots in this revision.

## Impact

This extends the drawer capability and the statusbar vocabulary, updates the
existing config example/help, and adds focused core/host tests. It follows the
accepted THE-11 architect design and remains compatible with the file-manager
provider seam and existing config trust rules.
