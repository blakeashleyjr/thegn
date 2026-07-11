# Terminals (Host / Remote Management) Feature Design

## Overview

This feature introduces a "Terminals" section into the thegn sidebar, bringing first-class management of isolated terminal environments (Local, SSH, Mosh) that exist outside of git worktrees. The goal is to let developers manage remote infrastructure and scratch environments in the same seamless way they manage branches, without shoehorning these shells into dummy git repos.

## User Experience

### Sidebar Appearance

```text
▼ My Repo (Workspace)
  · main
  · feat-branch
▼ Another Repo
  · main

▼ Terminals
  · Local          [Home/Scratch]
  · prod-web-01    [ssh user@10.0.0.5]
  · home-lab       [mosh my-server]
```

- A new top-level conceptual grouping called `Terminals` sits either at the bottom of the worktree list or below workspaces.
- Clicking a terminal row acts just like a worktree row:
  - Selects the terminal as the active "Group"
  - Reveals its Tabs in the bottom tab bar.
  - Spawns a shell/connection if not already running.

### Interactions

1. **Creation**: `Alt T` opens the Command Palette pre-filled with `> New Terminal Connection`.
   - User enters connection type (`local`, `ssh <host>`, `mosh <host>`).
   - Given a name.
   - Pushed into the `terminals` DB.
2. **Deletion**: `Alt Shift T` or pressing `Delete` on a terminal row in the sidebar removes it from the DB and closes its sessions.
3. **Usage**:
   - Terminals have normal multiplexing (tabs and panes).
   - "Local" shells drop the user in `$HOME` by default.
   - Remote shells execute the relevant connection binary instead of local `$SHELL`.

## Data Model Changes

### 1. `terminals` Database Table

```sql
CREATE TABLE IF NOT EXISTS terminals (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE,       -- Display name (e.g., "Local", "Web Server")
  kind TEXT NOT NULL,              -- "local", "ssh", "mosh", "container"
  connection_string TEXT NOT NULL, -- e.g. "user@hostname"
  folder_id INTEGER,               -- Optional for grouping
  created_at INTEGER NOT NULL,
  last_active INTEGER NOT NULL,
  position INTEGER                 -- Ordering within the list
);
```

### 2. `thegn-core/src/db.rs` API

- `put_terminal(...)`
- `terminals() -> Result<Vec<TerminalRow>>`
- `del_terminal(id: i64)`
- `rename_terminal(id: i64, new_name: &str)`
- `init_db()` migration: `PRAGMA user_version = 20` (or appropriate next version).

### 3. Core Structs Integration

#### `GroupKind`

```rust
pub enum GroupKind {
    Home,
    Branch,
    Terminal, // NEW
}
```

#### `RowKind`

```rust
pub enum RowKind {
    Workspace,
    Folder,
    Worktree,
    TerminalsHeader, // NEW
    Terminal,        // NEW
}
```

#### `SidebarRow` extensions

- Add `terminal_id: Option<i64>` to `SidebarRow` or handle it via the `workspace_slug` / `worktree_path` fields reusing string IDs. We likely want an explicit `terminal_id: Option<i64>` or `terminal_name: Option<String>`.

## Implementation Strategy

### M1: Persistence & Core Wiring

- Add the `terminals` DB schema and CRUD functions.
- Extend `GroupKind` and `RowKind`.
- No UI yet, just ensure `cargo test` passes.

### M2: Rendering the Sidebar

- Read terminals in `thegn-host/src/sidebar.rs` `build_rows()`.
- Append a `TerminalsHeader` row and the list of `Terminal` rows to the end of the sidebar.
- Style `TerminalsHeader` similar to a folder or top-level item.

### M3: Execution (PTY / Session)

- Update the PTY spawner (`crates/thegn-host/src/pane.rs` or `emulator.rs`) to inspect `GroupKind::Terminal`.
- When `Terminal`, execute based on the connection type.
- Handle graceful fallback / errors if `ssh` is unavailable.

### M4: Command Palette & Keybinds

- Implement the "New Terminal Connection" flow in the Palette.
- Add palette actions to Switch/Delete terminals.
- Wire up `Alt T`.

## Open Questions & Considerations

- **Sandboxing**: Should "local" terminals run in a `podman` sandbox if enabled, or always strictly local host? (Decision: Follow global sandbox config, but maybe provide an override later. Start simple: strictly host unless configured.)
- **Git Context**: `thegn` relies heavily on git metadata for PR counts, branch names, etc. We must ensure `Terminal` groups gracefully return empty/None for these queries without throwing errors.
- **Auto-provisioning**: Automatically create a "Local" terminal entry on first boot so users always have an escape hatch.
