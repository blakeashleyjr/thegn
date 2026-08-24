# Drawer

## ADDED Requirements

### Requirement: The drawer hosts a registry of occupants

The bottom drawer SHALL host an ordered registry of occupants: the built-in
**files** occupant (the file-manager drawer, resolved through its provider
seam) followed by each `[[drawer.tools]]` entry in config order. An entry MUST
either reference a `[[tools]]` entry by name (`tool = "<name>"`) or declare an
inline `command`, and MAY set a display `name`, a `cwd`, an `env` map, and a
`scope` of `worktree` (default) or `global`. Config validation MUST reject an
entry with both or neither of `tool`/`command`, and MUST warn on a `tool`
reference naming no `[[tools]]` entry (that occupant is omitted at runtime).
With no `[[drawer.tools]]` configured, drawer behavior SHALL be identical to
the single-occupant file drawer.

#### Scenario: An ATAC occupant opens in the drawer

- **WHEN** `[[tools]] name = "atac"` exists and a `[[drawer.tools]]` entry
  references `tool = "atac"` with `scope = "worktree"`, and the user switches
  the drawer to it
- **THEN** the drawer pane runs `atac` with cwd resolved against the active
  worktree and the entry's `env` applied

#### Scenario: A dangling tool reference degrades to a warning

- **WHEN** a `[[drawer.tools]]` entry references `tool = "nope"` and no
  `[[tools]]` entry has that name
- **THEN** config validation warns naming the entry, the occupant is absent
  from the drawer picker, and the remaining occupants work normally

#### Scenario: No registry entries means today's drawer

- **WHEN** no `[[drawer.tools]]` are configured
- **THEN** toggling the drawer opens the files occupant exactly as before,
  with no new chrome behavior

### Requirement: One visible occupant, switchable by global actions

The drawer SHALL show exactly one occupant at a time. The existing
`files-drawer` toggle SHALL open the worktree's last-open occupant (files when
none is remembered). Two bindable actions SHALL switch occupants —
`drawer-cycle` (next occupant in registry order) and `drawer-pick` (a
dedicated picker palette listing every occupant) — dispatched as chrome-level
chords, since a focused occupant owns every keystroke. Switching MUST stash
the outgoing occupant's pane in the keep-alive pool (state preserved) rather
than killing it, and an occupant process exiting on its own MUST close the
drawer, remove its pane, and clear the persisted open state.

#### Scenario: Cycling swaps the pane and keeps both alive

- **WHEN** the files occupant is open and the user invokes `drawer-cycle`
- **THEN** the next occupant's pane composites into the drawer rect, and
  toggling back to files restores its previous cursor/position

#### Scenario: The occupant quitting closes the drawer

- **WHEN** the visible occupant's process exits (e.g. `q` quits the tool)
- **THEN** the drawer closes, the pane is removed from pool and table, and the
  worktree's persisted drawer state is cleared

### Requirement: Worktree and global occupant scopes

A `scope = "worktree"` occupant SHALL run one pane per worktree with cwd at
the worktree root (or the entry's `cwd` resolved relative to it), pooled and
persisted per worktree. A `scope = "global"` occupant SHALL run a single
shared pane that follows the user across worktrees, with cwd at `$HOME` (or an
absolute/`~` `cwd`), pooled and persisted under one global slot. The pane
pool's `pool_limit` bound and eviction SHALL apply across all occupants keyed
by (scope key, occupant).

#### Scenario: A global scratch tool keeps its state across worktrees

- **WHEN** a `scope = "global"` occupant is open and the user switches to
  another worktree and reopens the drawer to that occupant
- **THEN** the same pane (same process, same screen state) is shown, not a new
  instance

#### Scenario: Worktree-scoped occupants stay per-worktree

- **WHEN** a `scope = "worktree"` occupant is open in worktree A and the user
  switches to worktree B
- **THEN** B shows its own persisted drawer state, and reopening the occupant
  in B spawns or restores B's instance, never A's

### Requirement: Persisted drawer state records the occupant

The per-worktree persisted drawer state SHALL record which occupant is open
(closed when none), remaining memory-first with write-through persistence off
the event loop. Legacy boolean flag files (`true`) MUST be read as the files
occupant so existing state survives the upgrade. No SQLite schema change is
involved.

#### Scenario: Restart restores the right occupant per worktree

- **WHEN** worktree A had the ATAC occupant open and worktree B had the drawer
  closed, and thegn restarts
- **THEN** switching to A reopens the drawer on ATAC and B stays closed

#### Scenario: Legacy flags mean files

- **WHEN** a pre-upgrade flag file containing `true` exists for a worktree
- **THEN** that worktree's drawer opens on the files occupant

### Requirement: A statusbar widget indicates the drawer

A `drawer` widget SHALL join the `[bars]` widget vocabulary (present in the
default `bottom_left` set) showing that the drawer exists and its state: a dim
glyph when closed, a highlighted glyph plus the active occupant's label when
open, and an occupant count when more than one is configured. Clicking the
widget SHALL toggle the drawer. The widget MUST be removable via `[bars]` like
any other widget, and its glyphs and colors MUST go through the caps/theme
chokepoints (no literals at the draw site).

#### Scenario: A closed drawer is discoverable

- **WHEN** the drawer is closed and the `drawer` widget is in `bottom_left`
- **THEN** the statusbar shows the dim drawer chip, and clicking it opens the
  drawer

#### Scenario: The open chip names the occupant

- **WHEN** the ATAC occupant is open
- **THEN** the chip renders highlighted with the occupant's label

### Requirement: Every occupant is contained and spawned off-loop

Every drawer occupant SHALL be wrapped in the `[drawer]` containment scope
(`contain`, `memory_max`, `memory_swap_max`, `cpu_quota`) with the same
fail-safe skips as the file manager (containment disabled, `systemd-run`
unavailable, or an already-wrapped argv pass through unchanged). Cold spawns
SHALL resolve off the event loop — request deduplicated per (scope key,
occupant), result delivered over a channel with a waker pulse — so opening or
switching occupants never blocks the loop.

#### Scenario: A runaway occupant is contained

- **WHEN** an occupant's process tree exceeds the drawer memory cap
- **THEN** it is OOM-killed inside its own scope and the terminal session
  survives

#### Scenario: Rapid switching does not duplicate spawns

- **WHEN** the user cycles quickly through a cold occupant several times
  before its spawn resolves
- **THEN** exactly one instance is spawned and later requests reuse it
