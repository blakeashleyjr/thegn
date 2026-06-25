# UX Polish: Diff-Exit Memory, Panel Toggle Fixes, Worktree Navigation

## Overview

Three independent UX improvements to the superzej terminal IDE:

1. **Diff-exit remembers cursor position** — Esc from file-diff view restores the file-list cursor
2. **Panel toggle fixes** — new `Ctrl Alt s`/`Ctrl Alt p` keybinds + pure pipe-based show/hide via `hide_self()`/`show_self(false)`
3. **Worktree navigation** — `Super Alt j/k` as parallel binds for `MoveFocus Up/Down`

---

## 1. Diff-Exit Remembers File-List Cursor

**State**: `plugin/panel/src/main.rs`

### Changes

1. Added `file_list_scroll: usize` to `State` struct (defaults to 0 via `#[derive(Default)]`)
2. In `on_file_list_key` (`Enter` handler): `self.file_list_scroll = self.diff_scroll` before `fetch_file_diff`
3. In `on_key` (Esc handler): `self.diff_scroll = self.file_list_scroll` instead of `= 0`
4. In `on_result` (`"files"` handler): removed `self.diff_scroll = 0` — the Esc handler already restores the saved position

No changes to the Rust binary, CLI, or layout files.

---

## 2. Panel Toggle Fixes

**Problems**:

- Existing `Alt s` (sidebar) and `Alt p` (panel) conflicted with zellij default binds
- `show_self(true)` restored the plugin as a floating/centered pane instead of in its original layout position
- `plugin_url()` used an expanded path (`file:/home/blake/...`) that didn't match the layout KDL's literal `~` path (`file:~/.local/...`), so `zellij pipe` couldn't find the running plugin

### Keybinding Changes (`config/zellij.kdl`)

```
bind "Ctrl Alt s" { Run "superzej" "sidebar" "--toggle"; }
bind "Ctrl Alt p" { Run "superzej" "panel" "--toggle"; }
```

`Ctrl Alt` modifier avoids zellij default conflicts (`Alt` + letter is heavily used).

### Root Cause — URL Mismatch

The `plugin_url()` function expanded `~` to `/home/blake/` when constructing the plugin URL (`file:/home/blake/.local/share/superzej/panel.wasm`). But the layout KDL files use a literal `~` (`file:~/.local/share/superzej/panel.wasm`). zellij identifies running plugins by their original layout URL string, so `pipe_plugin()` and `launch_or_focus_plugin()` couldn't find the plugin and spawned new floating instances instead.

**Fix**: `plugin_url()` now uses the literal `~` format, matching the layout KDL exactly.

### Toggle Mechanism (`src/commands/panels.rs`)

**State tracking**: A simple text file per surface at `~/.superzej/.sidebar_state` and `~/.superzej/.panel_state`, containing `true` (visible) or `false` (hidden). Initial state assumes visible (file absent = visible).

Both hide and show use `zellij pipe` to send a named message to the running plugin:

**Hide sequence** (visible → hidden):

1. `zellij pipe --plugin <url> --name superzej_toggle`
2. Plugin receives `superzej_toggle`, calls `hide_self()` — space is reclaimed
3. Binary writes `"false"` to the state file

**Show sequence** (hidden → visible):

1. `zellij pipe --plugin <url> --name superzej_show`
2. Plugin receives `superzej_show`, calls `show_self(false)` — restores in its original layout position, not as a floating pane
3. Binary writes `"true"` to the state file

The `false` argument to `show_self()` is critical: `show_self(true)` can cause the plugin to reappear as a floating/centered pane, while `show_self(false)` restores it in-place.

### Plugin Changes

Both `plugin/panel/src/main.rs` and `plugin/sidebar/src/main.rs`:

- `superzej_toggle` — existing toggle: `hide_self()` if visible, `show_self(true)` if hidden (legacy — `superzej_show` is the primary show path now)
- `superzej_show` — new: calls `show_self(false)` to restore in original layout position

---

## 3. Worktree Navigation

**Problem**: No keyboard shortcut to navigate between worktree panes (terminal panes) within a tab.

### Keybinding Changes (`config/zellij.kdl`)

Add alongside existing `Super Alt Up`/`Super Alt Down`:

```
bind "Super Alt j" { MoveFocus "Down"; }
bind "Super Alt k" { MoveFocus "Up"; }
```

These provide vim-style keys (j/k) alongside the arrow keys (↑/↓) for the same `MoveFocus` action. Within the center column of a worktree tab, `MoveFocus Up/Down` navigates between the 1-row tabbar and the terminal pane(s).

No binary, plugin, or layout file changes needed.

---

## Files Changed

| File                         | Change                                                                                                           |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `plugin/panel/src/main.rs`   | Added `file_list_scroll` field + save/restore logic (section 1); added `superzej_show` pipe handler (section 2)  |
| `plugin/sidebar/src/main.rs` | Added `superzej_show` pipe handler (section 2)                                                                   |
| `src/commands/panels.rs`     | Rewrote toggle: state file tracking, pure pipe-based show/hide, fixed URL format to match layout KDL (section 2) |
| `config/zellij.kdl`          | Replaced `Alt s`/`Alt p` with `Ctrl Alt s`/`Ctrl Alt p`; added `Super Alt j`/`Super Alt k` (sections 2 & 3)      |

## Risks & Mitigations

- **Ctrl+Alt modifier**: Some terminal emulators intercept `Ctrl+Alt` combos. `support_kitty_keyboard_protocol true` in zellij.kdl mitigates this. If it doesn't work, rebind to a simpler chord.
- **State file staleness**: If the plugin pane is closed by other means (zellij action, crash), the state file says `false` (hidden) but the plugin is gone. Next toggle tries `pipe_plugin` which can't find it — zellij spawns a floating instance. Mitigation: not implemented yet — user can cycle toggle twice to reset state.
