# Workspaces ⇄ Terminals region navigation

Date: 2026-06-28

## Problem

The sidebar shows two stacked regions — **workspaces → worktrees** (top) and a
first-class **TERMINALS → host → terminal** section (bottom). But the _global_
motion keys only ever move within the workspaces region:

- `Alt+↑/↓` cycles worktrees within the active workspace (`run.rs` `NextWorktree`).
- `Shift+Alt+↑/↓` cycles workspaces (`run.rs` `NextWorkspace`).

There is no fast way to jump to a terminal, no way to bounce back to where you
were, and `Shift+Alt` stops dead at the workspace boundary. Reaching a terminal
means `Alt+s` then arrowing the sidebar cursor past everything.

## Model

Terminals already live in the **same `session.worktrees` vector** as worktrees,
distinguished by `GroupKind::Terminal`. So "which region am I in" is simply the
active group's kind. The navigable space is two peer regions, each two levels:

| Region         | Container  | Leaf     |
| -------------- | ---------- | -------- |
| W (workspaces) | workspace  | worktree |
| T (terminals)  | host group | terminal |

The `TERMINALS` banner is a non-navigable heading. A default `local` terminal
always exists (`run.rs:622`), so Region T is never empty.

## Behavior

### `Alt+↑/↓` — leaf cycle (now region-aware)

- Region W (unchanged): cycle worktrees within the active workspace.
- Region T (new): cycle terminals within the active host, in sidebar display
  order, wrapping within the host.

### `Shift+Alt+↑/↓` — container ring with W↔T overflow

Walks one combined ring: `[switchable workspaces…] + [terminal hosts…]`, wrapping
as a single cycle. At the last workspace, `Shift+Alt+↓` descends into the first
terminal host; from the first host, `Shift+Alt+↑` climbs back to the last
workspace. Landing on a workspace switches workspace (existing path); landing on
a host activates that host's first terminal and expands the host group.

### `` Alt+` `` — region toggle (remembers place)

Bounces between regions, restoring each region's remembered position:

- In a worktree → jump to your last terminal (or the first terminal if none yet).
- In a terminal → jump back to your last worktree (or the home worktree).

### Place memory

Two event-loop fields:

- `region_last_w: Option<usize>` — last worktree group index (validated against
  the current session at use; reset on a workspace switch in these handlers).
- `region_last_t: Option<String>` — last terminal **name** (robust across
  workspace switches; re-materializes via the sentinel activation path).

Refreshed after every region nav from the resulting active group.

### Activation semantics

All motions activate the target as the center tab (`focus.zone = Center`, sidebar
cursor follows). Terminals route through the existing
`activate_row_target(RowTarget::Workspace { repo_path: "terminal", group })`
path, which switches to a resident terminal group or materializes one
(spawn-on-activate) — so non-resident terminals just work.

## Implementation

- `sidebar.rs`: extract `terminal_hosts_ordered(db_terminals)` (host grouping +
  `local`-first sort), shared by `build_rows` and the nav helpers.
- `keymap.rs`: `Action::ToggleRegion` (`"toggle-region"`), `ACTION_SPECS` entry,
  default bind `` Alt+` ``, key/from_key round-trip + a binding test.
- `run.rs`: `region_last_w`/`region_last_t` state; `active_is_terminal` +
  `active_terminal_host_key` helpers; terminal-aware branch in the
  `NextWorktree/PrevWorktree` arm; combined-ring rewrite of the
  `NextWorkspace/PrevWorkspace` arm; new `ToggleRegion` arm.
- `config/config.toml.example`: document the binding.

Non-goals (YAGNI): persisting region memory across restart; per-host last-leaf
memory beyond the single `region_last_t`.
