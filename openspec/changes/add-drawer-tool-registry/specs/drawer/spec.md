# Drawer

## ADDED Requirements

### Requirement: The drawer hosts the effective tools registry

The bottom drawer SHALL host the built-in files occupant followed by each
eligible `[[tools]]` entry in config order. A tool is eligible only when it
has valid `drawer_scope = "worktree"` or `drawer_scope = "global"`; its
existing `name`, `command`, and `env` remain authoritative, and `drawer_cwd`
is optional. Tools without drawer metadata remain picker-only. Strict config
validation SHALL report malformed drawer metadata, while normal loading SHALL
omit only the invalid occupant and warn.

#### Scenario: A configured ATAC tool is listed

- **WHEN** `[[tools]] name = "atac"`, `command = "atac"`, and
  `drawer_scope = "worktree"` are configured
- **THEN** the drawer picker lists `tool:atac` after `files`

#### Scenario: A global tool is reachable from worktree chrome

- **WHEN** a tool has `drawer_scope = "global"` and the active scope is a
  worktree
- **THEN** cycle and picker include that tool and selection persists it under
  the fixed global state key

### Requirement: One visible occupant uses one runtime boundary

The drawer SHALL show one occupant at a time. `files-drawer`, `drawer-cycle`,
and `drawer-pick` SHALL share one runtime for selection, switching, pooling,
persistence, async results, process exit, and geometry. Switching SHALL stash
the outgoing pane under `(scope-key, occupant-id)` rather than creating a
second lifecycle. A process exit SHALL remove the pane and clear its matching
state.

#### Scenario: A visible drawer survives a worktree switch

- **WHEN** a drawer occupant is visible and the active worktree changes
- **THEN** the runtime stashes or restores the correct scoped pane, with an
  open destination worktree occupant taking precedence over a global one

#### Scenario: Prewarm does not create a second drawer lifecycle

- **WHEN** `[drawer].prewarm = true` and a configured occupant is selected
- **THEN** only the runtime's files-only prewarm path may request a files pane;
  no legacy drawer request or pool is used

### Requirement: Scope and persistence are explicit

Worktree occupants SHALL use one pane and state slot per worktree. Global
occupants SHALL use one process-local local PTY and the fixed global state slot
across in-process worktree switches. Global panes SHALL not be daemon-owned or
restored after restart. Legacy `true` state SHALL decode as `files`, and
`false` SHALL decode as closed.

### Requirement: Occupants are contained and resolved off-loop

Every occupant SHALL use the existing drawer argv conversion, containment,
and local-spawn seams. Cold resolution, environment expansion, and PATH or
filesystem checks SHALL happen off the event loop through a channel and waker,
deduplicated by `(scope-key, occupant-id)`; stale results SHALL be dropped.

### Requirement: The drawer indicator is discoverable and removable

The `drawer` bars widget SHALL be present in the default `bottom_left` order,
use existing glyph/theme chokepoints, show closed/open occupant state and the
configured occupant count, and toggle the same files-drawer action when
clicked. Removing it from `[bars]` SHALL remove both its paint and hit target.
