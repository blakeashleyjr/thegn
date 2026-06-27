# Tasks

## 1. Persistence (superzej-core)

- [ ] 1.1 `terminals` table + `put_terminal`/`terminals`/`del_terminal`/
      `rename_terminal`; `user_version` bump — **unit tests** (CRUD round-trip,
      isolated `XDG_STATE_HOME`).
- [ ] 1.2 Extend `GroupKind` (`Terminal`) and sidebar `RowKind`
      (`TerminalsHeader`, `Terminal`) — **unit tests**.

## 2. Sidebar (host)

- [ ] 2.1 `build_rows` appends the Terminals header + rows; git-only queries
      return empty/None for terminal groups.

## 3. Execution (host)

- [ ] 3.1 PTY spawner branches on `GroupKind::Terminal` (local `$HOME`; ssh/mosh
      exec connection binary); graceful error when the binary is absent.

## 4. Palette + keybinds (host)

- [ ] 4.1 `Alt T` new-terminal flow; `Alt Shift T`/`Delete` removal; auto-provision
      a "Local" terminal on first boot.

## 5. Validate

- [ ] 5.1 Run `just ci` (includes `openspec-validate`).
