# Terminals (Host / Remote Management) Feature Plan

## Context

thegn currently treats git worktrees and repos as the primary organizational unit. Workspaces hold worktrees. However, developers often need raw terminal access to systems (local machines, remote servers, cloud environments) that aren't tied to a specific repo or git worktree.

This plan details a new section under "Workspaces" called "Terminals" that provides a managed list of local and remote terminal sessions, starting with `local` and scaling to SSH/Mosh environments. This allows users to handle ops and general terminal tasks without forcing them into a git worktree structure.

## Architecture

1. **Database Layer (`crates/thegn-core/src/db.rs`)**
   - New table: `terminals`
     - `id` INTEGER PRIMARY KEY
     - `name` TEXT (e.g. "Local", "prod-web-01")
     - `kind` TEXT ("local", "ssh", "mosh", "container")
     - `connection_string` TEXT (e.g. "user@host:22")
     - `folder_id` INTEGER (optional grouping, references `folders` table)
     - `created_at` INTEGER
     - `last_active` INTEGER
     - `position` INTEGER
   - Add DB methods: `put_terminal()`, `terminals()`, `del_terminal()`, `rename_terminal()`.
   - Update `init_db()` to bump `user_version` and create the `terminals` table.

2. **Sidebar Model (`crates/thegn-host/src/sidebar.rs`)**
   - `RowKind` enum addition: `TerminalsHeader`, `Terminal`
   - Update `SidebarRow` to handle terminal-specific data (e.g. a `terminal_id` field).
   - Render a "Terminals" section below Workspaces.
   - "Local" is always the first default terminal (seeded on DB init or dynamically resolved if missing).

3. **Session & Process Management (`crates/thegn-host/src/session.rs`)**
   - `GroupKind` enum extension: Add `Terminal` to the existing `Home` and `Branch` variants.
   - We will need a way to store the `terminal_id` or connection string in the group so it knows how to spawn panes. The easiest approach is passing the `terminal_id` as the `path` or adding a new field to `WorktreeGroup` like `terminal_info: Option<TerminalInfo>`.
   - Update `WorktreeGroup` to represent a generic tab group that might be a terminal group rather than a strict git worktree. We might want to rename `WorktreeGroup` to something more generic like `TabGroup` or create a new struct for terminal sessions, but adapting `WorktreeGroup` with `GroupKind::Terminal` is likely the most straightforward path since it maps to the UI tab bar directly.

4. **Execution Layer (`crates/thegn-host/src/pane.rs` & `emulator.rs`)**
   - PTY backend updates to spawn raw shells or SSH/Mosh subprocesses directly instead of setting `CWD` to a worktree path.
   - For `kind="local"`, spawn the default shell (`$SHELL` or fallback) in `$HOME`.
   - For `kind="ssh"`, spawn `ssh <connection_string>`.
   - Ensure the standard multiplexing models (Pane, Tab) work gracefully without git context hooks triggering errors.

5. **Action & Palette Integration (`crates/thegn-host/src/chrome.rs`, `center.rs`)**
   - New palette commands: "New Terminal", "Connect to Remote Server"
   - Keybinds for quickly spinning up a scratch terminal (e.g. `Alt T` for terminal).

## Implementation Phases

### Phase 1: Database & Core Models

- [ ] DB Schema: Increment `PRAGMA user_version` and create `terminals` table in `init_db`.
- [ ] DB Operations: Implement `terminals()`, `put_terminal()`, `del_terminal()`, `rename_terminal()` in `db.rs`.
- [ ] Initialize with a default "Local" terminal entry if the table is empty.

### Phase 2: Session & UI Models

- [ ] Extend `GroupKind` with `Terminal` in `session.rs`.
- [ ] Extend `RowKind` with `TerminalsHeader` and `Terminal` in `sidebar.rs`.
- [ ] Update `sidebar.rs:build_rows()` to fetch terminals from the DB and render the new section at the bottom of the sidebar.

### Phase 3: Host Execution

- [ ] Extend PTY / Pane creation logic in `pane.rs` / `emulator.rs` to recognize `GroupKind::Terminal`.
- [ ] Implement local shell spawning without requiring a valid git repo path.
- [ ] Implement remote connections (`ssh`, `mosh`) using native fallback subprocess execution (leveraging `thegn-svc` if applicable, or direct `Command::new`).

### Phase 4: UI & Palette Polish

- [ ] Add "New Terminal Connection" flow in palette/actions to prompt for host strings.
- [ ] Support renaming and deleting terminal entries from the sidebar (using standard interaction patterns).
- [ ] Add visual polish (icons for local vs remote, e.g. `💻` and `🌐`).
