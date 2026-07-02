# Design

## Data model

New `terminals` DB table (`id, name UNIQUE, kind, connection_string, folder_id,
created_at, last_active, position`) with `put_terminal`/`terminals`/`del_terminal`/
`rename_terminal` in `db.rs` and a `user_version` bump. `GroupKind` gains
`Terminal`; sidebar `RowKind` gains `TerminalsHeader` + `Terminal`.

## Sidebar + execution

`sidebar.rs::build_rows` appends a `TerminalsHeader` and the terminal rows after
the worktrees. Selecting a terminal row behaves like a worktree row (active group,
its tabs in the tabbar, spawn-if-not-running). The PTY spawner inspects
`GroupKind::Terminal`: `local` drops into `$HOME`; `ssh`/`mosh` exec the connection
binary instead of `$SHELL`, with graceful error if the binary is missing.

## Palette + keybinds

`Alt T` opens the palette prefilled with "New Terminal Connection" (kind + name);
`Alt Shift T` / `Delete` on a terminal row removes it. Git-dependent queries (PR
counts, branch) return empty/None for terminal groups rather than erroring. A
"Local" terminal is auto-provisioned on first boot as an escape hatch.

## Invariants

git stays the source of truth for worktrees; terminals are a separate DB-backed
list. No polling added; spawn/connect runs through the existing PTY seam.
