# Design — drawer tool registry

THE-11 extends the existing `[[tools]]` catalog. A tool with
`drawer_scope = "worktree"` or `drawer_scope = "global"` becomes an eligible
drawer occupant; `name`, `command`, and `env` remain the single launch record.
`drawer_cwd` is optional and is relative to the worktree for worktree scope,
or absolute/`~`-prefixed for global scope. The built-in files provider is the
first occupant, followed by eligible tools in config order.

There is deliberately no `[[drawer.tools]]` table, inline-command syntax, or
repo-local command registry. This avoids duplicating the picker catalog and
keeps command, environment, and trust handling on the existing paths. Invalid
drawer metadata is reported by strict validation and omitted with a warning by
normal layered loading; tools without drawer metadata remain picker-only.

## Runtime

`DrawerRuntime` is the sole owner of drawer transitions, persistence, pooling,
async resolution, process-exit cleanup, and geometry updates. The pool is
keyed by `(scope-key, occupant-id)` and the configured `pool_limit` applies
across all occupants. Worktree state uses the existing slugged directory key;
global state uses the fixed `global` key. Legacy `true` flags decode as
`files`, and `false` is closed.

Only one occupant is visible. A destination worktree's open occupant takes
precedence over an open global occupant; a global occupant reuses one local
PTY across in-process worktree switches. It is not daemon-owned and does not
survive detach or restart. Cold resolution and environment expansion happen
off-loop through the existing channel+waker boundary. Every occupant uses
`tool_drawer_argv`, `contain_drawer_argv`, and `spawn_argv_env_local`.

`files-drawer`, `drawer-cycle`, and the dedicated `drawer-pick` palette all
use the same runtime transition path. The picker and cycle registry contain
both worktree and global tools. Process exit removes the pane and clears its
matching state slot. `[drawer].prewarm` remains files-only and also goes
through `DrawerRuntime`.

## Indicator and scope

The removable `drawer` statusbar widget is in the default `bottom_left`
order. It uses the existing glyph/theme chokepoints, shows a dim closed state,
or an accented active occupant label and count, and clicking it invokes the
files-drawer toggle. It is painted and hit-tested from the same layout item.

No CLI, daemon, control API, MCP, plugin, SQLite migration, or new capability
catalog entry is added. Global panes are process-local ephemeral chrome.

## Verification and follow-up

Core policy/runtime behavior is covered by focused unit tests; host picker,
switch, indicator, containment, and lifecycle behavior have targeted tests.
The eight chrome snapshots listed in the architect design are a deferred
follow-up and are not re-recorded in this coder revision. Full CI and e2e are
also outside this revision's mandated dev loop.
