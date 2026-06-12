# Workspace Create & Delete Workflow — Implementation Plan

## Current State Assessment

### What's working (native host / szhost)

| Component                                                                    | Status                     |
| ---------------------------------------------------------------------------- | -------------------------- | ----------- |
| DB: `workspaces` table (repo_path, name, created_at, last_active)            | ✅ Complete                |
| DB: `put_workspace()` upsert                                                 | ✅ Complete                |
| DB: `workspaces()` list (by last_active DESC)                                | ✅ Complete                |
| DB: `slug_for_repo()` with collision handling                                | ✅ Complete                |
| DB: `touch_repo()` / `recent_repos()` for recents                            | ✅ Complete                |
| DB: `known_repos()` union across workspaces/worktrees/repos                  | ✅ Complete                |
| DB: `is_known_repo()` exists check                                           | ✅ Complete                |
| CLI: `superzej new-workspace [path                                           | url]` (legacy zellij path) | ✅ Complete |
| Session: `switch_to_workspace()` persist-old → resurrect-new → seed-if-empty | ✅ Complete                |
| Sidebar: builds workspace list from `db.workspaces()` + session tabs         | ✅ Complete                |
| Palette: "new-workspace", "switch-workspace" entries                         | ✅ Exists                  |
| Palette: workspace entries (✦ repo_name) as switch targets                   | ✅ Complete                |
| Keybind: `Alt W` → `NewWorkspace`, `Alt o` → `SwitchWorkspace`               | ✅ Complete                |
| Keybind: `Alt x` → `CloseTab`, `Alt X` → `CloseWorktree`                     | ✅ Complete                |

### What's partially implemented

| Component                          | Gap                                                                                                              |
| ---------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Native host NewWorkspace action    | Only opens palette, requires user to **already have a registered workspace**. No _creation_ flow in native host. |
| Native host SwitchWorkspace action | Same flow as NewWorkspace — both open palette.                                                                   |
| `Alt W` / palette "New workspace"  | Falls through to the same palette-based switch flow. No path entry, no git clone, no repo discovery.             |

### What's completely missing

| Component                                                                | Priority |
| ------------------------------------------------------------------------ | -------- |
| **DB: `delete_workspace()`** — remove a workspace row                    | P0       |
| **Action: `DeleteWorkspace`** — new host action                          | P0       |
| **Keybind: `Alt Shift X`** → DeleteWorkspace                             | P0       |
| **Create workspace dialog** — inline UI for path/URL entry               | P0       |
| **Delete workspace with confirmation** — sidebar/palette interaction     | P0       |
| **Orphaned worktree detection** — warn before delete if worktrees exist  | P1       |
| **Workspace creation feedback** — status bar message after create/delete | P1       |
| **Sidebar delete key** — `Delete` / `d` on selected workspace row        | P1       |
| **Garbage-collect stale worktrees** on delete (optional)                 | P2       |
| **Create from repo root dirs** (item #31) — browse configured roots      | P2       |
| **Clone from URL dialog** (items #29 impl polish)                        | P2       |

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        superzej (szhost)                         │
│                                                                   │
│  ┌──────────┐  ┌──────────────┐  ┌─────────────┐               │
│  │ Sidebar  │  │   Palette    │  │  Keymap      │               │
│  │ (j/k/Ent)│  │ (Cmd-K)      │  │ (Alt-Shift-X)│               │
│  └────┬─────┘  └──────┬───────┘  └──────┬──────┘               │
│       │               │                │                        │
│       └───────────────┼────────────────┘                        │
│                       │                                          │
│                       ▼                                          │
│            ┌──────────────────────┐                             │
│            │   Action Dispatch    │                             │
│            │  (run.rs event_loop) │                             │
│            └──────────┬───────────┘                             │
│                       │                                          │
│         ┌─────────────┼──────────────┐                          │
│         ▼             ▼              ▼                          │
│  ┌───────────┐ ┌────────────┐ ┌──────────────┐                 │
│  │  Create   │ │  Delete    │ │   Switch     │                 │
│  │ Workspace │ │ Workspace  │ │  Workspace   │                 │
│  └─────┬─────┘ └─────┬──────┘ └──────┬───────┘                 │
│        │             │              │                           │
│        └─────────────┼──────────────┘                           │
│                      ▼                                           │
│            ┌──────────────────┐                                 │
│            │  Session + DB    │                                 │
│            │  (session.rs +   │                                 │
│            │   core/db.rs)    │                                 │
│            └──────────────────┘                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## Implementation Plan

### Phase 1: Core DB + Session Infrastructure (P0)

#### 1.1 Add `delete_workspace()` to `crates/superzej-core/src/db.rs`

```rust
/// Remove a workspace registration. This is non-destructive: it only
/// removes the row from the `workspaces` table. Worktrees on disk are
/// untouched. Returns the count of known worktrees under this repo
/// so the caller can warn about orphaned trees.
pub fn delete_workspace(&self, repo_path: &str) -> Result<u32> {
    // Count orphaned worktrees before deleting
    let orphan_count: u32 = self.conn.query_row(
        "SELECT COUNT(*) FROM worktrees WHERE repo_path = ?1",
        params![repo_path],
        |r| r.get(0),
    ).unwrap_or(0);

    self.conn.execute(
        "DELETE FROM workspaces WHERE repo_path = ?1",
        params![repo_path],
    )?;
    // Also clean up repo_slugs (optional — keeps slug reserved
    // in case it's re-added, but we could also delete)
    // Leave slug for now — non-destructive

    Ok(orphan_count)
}
```

**Files touched:** `crates/superzej-core/src/db.rs`

- Add method `delete_workspace(&self, repo_path: &str) -> Result<u32>`
- Add unit test `workspace_delete_removes_row_and_reports_orphans()`

#### 1.2 Add `DeleteWorkspace` to host keymap

**Files touched:** `crates/superzej-host/src/keymap.rs`

- Add `DeleteWorkspace` variant to `Action` enum
- Add `"delete-workspace"` key to `Action::key()` / `Action::from_key()`
- Add default binding: `"Alt Shift X"` → `Action::DeleteWorkspace`
- Add vim-normal binding: `"Space X"` → `Action::DeleteWorkspace`

#### 1.3 Add `DeleteWorkspace` dispatch in event loop

**Files touched:** `crates/superzej-host/src/run.rs`

- Add `Action::DeleteWorkspace` arm in the matched action dispatch (inside `crate::sequence::MatchResult::Matched(action)` block)
- Logic:
  1. Open DB
  2. Call `db.delete_workspace(&session.id)`
  3. If orphan count > 0: display status warning ("⚠ Deleted workspace. N orphaned worktrees remain at ~/.superzej/worktrees/...")
  4. Switch to the next available workspace (first from `db.workspaces()`)
  5. Refresh sidebar
- Add palette entry: `PaletteItem::new("delete-workspace", "Delete workspace ✕")`

---

### Phase 2: Create Workspace Dialog (P0)

#### 2.1 Distinguish NewWorkspace from SwitchWorkspace

Currently both `NewWorkspace` and `SwitchWorkspace` open the palette with workspace items. We need `NewWorkspace` to offer a **creation flow** while `SwitchWorkspace` keeps the existing picker.

**Approach:** Add a simple inline prompt in the native host. Because we don't have a widget system yet, use a **status-bar prompt** pattern:

- When `Action::NewWorkspace` fires, set `create_workspace_mode: bool` on the event loop state
- Show prompt in status bar: `"Create workspace — enter path or URL (Esc to cancel)"`
- Key input goes to an accumulating prompt buffer until Enter/Esc

**Files touched:** `crates/superzej-host/src/run.rs`

- Add `prompt: Option<String>` to the event loop mutable state
- Add `prompt_mode: PromptMode` enum (`None`, `NewWorkspace`, `DeleteConfirm`)
- Implement prompt handling in the input loop (before palette/keymap dispatch):
  - Characters append to prompt buffer
  - Enter commits, Esc cancels
  - Backspace deletes last char

```rust
enum PromptMode {
    None,
    NewWorkspace,
    DeleteConfirm,
}

// In event_loop state:
let mut prompt: Option<String> = None;
let mut prompt_mode = PromptMode::None;

// In input handling, BEFORE palette check:
if let Some(ref mut buf) = prompt {
    match k.key {
        KeyCode::Escape => { prompt = None; prompt_mode = PromptMode::None; dirty = true; continue; }
        KeyCode::Enter => {
            let input = buf.clone();
            prompt = None;
            match prompt_mode {
                PromptMode::NewWorkspace => {
                    // Dispatch create with `input`
                    match create_workspace_from_input(&input, &mut session, &db) {
                        Ok(()) => {
                            model.status = format!("workspace '{}' created", input);
                            need_relayout = true;
                        }
                        Err(e) => {
                            model.status = format!("error: {}", e);
                        }
                    }
                }
                PromptMode::DeleteConfirm => {
                    // Confirm delete
                    match db.delete_workspace(&session.id) {
                        Ok(orphans) => { /* switch away */ }
                        Err(e) => { /* show error */ }
                    }
                }
                _ => {}
            }
            prompt_mode = PromptMode::None;
            dirty = true;
            continue;
        }
        KeyCode::Backspace => { buf.pop(); dirty = true; continue; }
        KeyCode::Char(c) if !k.modifiers.contains(Modifiers::CTRL) => {
            buf.push(c);
            dirty = true;
            continue;
        }
        _ => {}
    }
}
```

#### 2.2 Create workspace logic

```rust
fn create_workspace_from_input(
    input: &str,
    session: &mut Session,
    db: &Db,
) -> Result<()> {
    let input = input.trim();
    if input.is_empty() {
        return Err(anyhow::anyhow!("no path given"));
    }

    let root = if superzej_core::util::is_url(input) {
        // Clone from URL
        let repo_name = superzej_core::util::basename(input)
            .trim_end_matches(".git").to_string();
        let dest = superzej_core::config::workspaces_dir().join(&repo_name);
        if !dest.join(".git").is_dir() {
            std::fs::create_dir_all(dest.parent().unwrap())?;
            let status = std::process::Command::new("git")
                .arg("clone").arg(input).arg(&dest)
                .status()?;
            if !status.success() {
                return Err(anyhow::anyhow!("clone failed"));
            }
        }
        dest
    } else {
        let p = std::path::Path::new(input);
        if !p.is_dir() {
            return Err(anyhow::anyhow!("path does not exist: {}", input));
        }
        if !p.join(".git").is_dir() {
            return Err(anyhow::anyhow!("not a git repository: {}", input));
        }
        p.to_path_buf()
    };

    let root_s = root.to_string_lossy().into_owned();
    let name = superzej_core::repo::repo_name(&root);
    db.put_workspace(&root_s, &name)?;
    db.touch_repo(&root_s, &name)?;

    // Switch to the new workspace
    session.switch_to_workspace(&root_s, db)?;

    Ok(())
}
```

**Files touched:** `crates/superzej-core/src/util.rs` (add `is_url()` if not present), `crates/superzej-host/src/run.rs`

---

### Phase 3: Delete Workspace UX (P0-P1)

#### 3.1 Delete from sidebar

Add sidebar key handling when a workspace row is selected (not a worktree child):

- `d` key → start delete confirmation
- `x` key → start delete confirmation
- `Backspace` → start delete confirmation

The sidebar needs to distinguish between workspace headers and worktree children:

```rust
// In sidebar navigation (run.rs, sidebar_focused block):
KeyCode::Char('d') | KeyCode::Char('x') => {
    // Check if selected row is a workspace (not a child worktree)
    if is_workspace_row(&model, model.sidebar_selected) {
        // Start delete confirmation
        prompt_mode = PromptMode::DeleteConfirm;
        prompt = Some(String::new());
        model.status = "Delete this workspace? Type 'yes' to confirm, Esc to cancel".into();
    }
    dirty = true;
    continue;
}
```

#### 3.2 Delete from palette

Add `"delete-workspace"` palette item that triggers the delete confirmation flow.

#### 3.3 Confirm + after-delete flow

```rust
// On Enter with prompt_mode == DeleteConfirm:
if prompt.as_deref() == Some("yes") {
    match db.delete_workspace(&session.id) {
        Ok(orphan_count) => {
            // Find next workspace to switch to
            if let Ok(workspaces) = db.workspaces() {
                if let Some(next) = workspaces.first() {
                    let _ = session.switch_to_workspace(&next.repo_path, &db);
                } else {
                    // No workspaces left — create empty home
                    session.id.clear();
                    session.tabs.clear();
                    session.active = 0;
                }
            }
            refresh_tab_model(&mut model, &session);
            need_relayout = true;
            if orphan_count > 0 {
                model.status = format!(
                    "⚠ Deleted workspace. {} orphaned worktree(s) remain at ~/.superzej/worktrees/",
                    orphan_count
                );
            } else {
                model.status = "Workspace deleted".into();
            }
        }
        Err(e) => {
            model.status = format!("delete failed: {}", e);
        }
    }
} else {
    model.status = "Delete cancelled".into();
}
```

---

### Phase 4: Polish & Edge Cases (P1-P2)

#### 4.1 Edge cases

| Case                                                  | Handling                                                                                                                                       |
| ----------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| Delete while active workspace has no other workspaces | Create minimal empty home tab (no repo). Show status: "Last workspace deleted. Press Alt-W to add a workspace."                                |
| Delete with unsaved pane work                         | Should warn before switching. Currently `switch_to_workspace` calls `persist()` — ok.                                                          |
| Create duplicate workspace                            | `put_workspace` is an upsert — silently re-adds. Show status: "Workspace already registered — switched to it."                                 |
| Create offline / no db                                | `Db::open()` returns `Err` — catch and show "Cannot create workspace: database unavailable"                                                    |
| Create with git URL that fails to clone               | Catch clone failure, show "Clone failed: <reason>"                                                                                             |
| Delete from sidebar with Enter key                    | Should NOT delete — only act on confirmation. Enter on workspace row should switch to it.                                                      |
| Prompt mode conflicts                                 | Only allow one prompt at a time. Ignore palette open during prompt.                                                                            |
| Refresh after remote delete                           | If workspace dir deleted from filesystem, sidebar still shows it. Add existence check in sidebar builder (gray out / warn). Deferred to later. |

#### 4.2 Status bar feedback pattern

The prompt buffer should render in the status bar during prompt mode:

```rust
// In render section (after chrome paint, before flush):
if let Some(ref prompt_text) = prompt {
    let status = format!("{} {}", match prompt_mode {
        PromptMode::NewWorkspace => "Create workspace:",
        PromptMode::DeleteConfirm => "Delete workspace? Type 'yes':",
        PromptMode::None => "",
    }, prompt_text);
    // Override model.status temporarily for rendering
}
```

---

## Keystroke Flow Diagrams

### Create Workspace

```
User presses Alt-W (NewWorkspace)
  → Action::NewWorkspace dispatched
  → Set prompt_mode = PromptMode::NewWorkspace, prompt = Some("")
  → Status bar shows "Create workspace: "
  → User types path (e.g., "/home/blake/code/myproject")
  → User presses Enter
  → create_workspace_from_input("/home/blake/code/myproject", &session, &db)
    → Validate: path exists, is a git dir
    → db.put_workspace(path, name)
    → db.touch_repo(path, name)
    → session.switch_to_workspace(path, db)
      → persist current session tabs
      → resurrect new workspace tabs (or create {slug}/home)
      → set session.id = new path
    → refresh_tab_model → sidebar rebuilds
    → need_relayout = true
    → model.status = "workspace 'myproject' created"
  → OR user presses Esc
    → prompt = None, prompt_mode = None
    → model.status = "cancelled"
```

### Delete Workspace

```
User presses Alt-Shift-X (DeleteWorkspace)
  OR selects "Delete workspace" in palette
  OR presses 'd' while focus is on workspace row in sidebar
  → Set prompt_mode = PromptMode::DeleteConfirm, prompt = Some("")
  → Status bar shows "Delete workspace? Type 'yes' to confirm, Esc to cancel"
  → User types 'yes' + Enter
  → db.delete_workspace(&session.id)
    → DELETE FROM workspaces WHERE repo_path = ?
    → Returns orphan worktree count
  → Switch to next workspace (first in db.workspaces())
    OR if none: create empty tab
  → refresh_tab_model
  → need_relayout = true
  → model.status = "Workspace deleted" (or "⚠ N orphaned worktrees remain")
  → OR user types anything else + Enter
    → model.status = "Delete cancelled"
  → OR user presses Esc
    → model.status = "Delete cancelled"
```

### Switch Workspace

```
User presses Alt-o (SwitchWorkspace)
  → Opens palette with workspace list (✦ repo_name for each workspace)
  → User selects a workspace
  → session.switch_to_workspace(repo_path, db)
  → refresh_tab_model
  → need_relayout = true
```

---

## File Change Summary

### New files

- None (all changes in existing files)

### Modified files

| File                                  | Changes                                                                                                                                                                                                     |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/superzej-core/src/db.rs`      | Add `delete_workspace(&self, repo_path: &str) -> Result<u32>`. Add unit test.                                                                                                                               |
| `crates/superzej-core/src/util.rs`    | Add `is_url(s: &str) -> bool` helper (moved from CLI crate).                                                                                                                                                |
| `crates/superzej-host/src/keymap.rs`  | Add `DeleteWorkspace` action, keybind (`Alt Shift X`, `Space X`).                                                                                                                                           |
| `crates/superzej-host/src/run.rs`     | Add prompt mode system. Add `Action::DeleteWorkspace` dispatch. Rework `Action::NewWorkspace` to use prompt (not palette). Add sidebar delete keys. Add `create_workspace_from_input()`. Add palette items. |
| `crates/superzej-host/src/session.rs` | No changes needed (switch_to_workspace already works).                                                                                                                                                      |
| `crates/superzej-host/src/chrome.rs`  | Add status bar rendering for prompt mode when visible.                                                                                                                                                      |

### Test changes

| Test                                               | Type             |
| -------------------------------------------------- | ---------------- |
| `workspace_delete_removes_row_and_reports_orphans` | Unit (db.rs)     |
| `delete_workspace_keybind_maps_correctly`          | Unit (keymap.rs) |
| `prompt_creation_switches_to_new_workspace`        | Unit (run.rs)    |
| `prompt_cancellation_returns_to_normal_mode`       | Unit (run.rs)    |
| `delete_last_workspace_creates_empty_tab`          | Unit (run.rs)    |

---

## Migration / Backward Compatibility

- `put_workspace` already handles idempotent re-creation (upsert on `repo_path`)
- `delete_workspace` is non-destructive — only removes DB row, worktrees on disk survive
- No schema migration needed (no new columns)
- Legacy zellij path (`crates/superzej-cli/src/commands/new_workspace.rs`) is unaffected
- Existing keybindings preserved; only additive changes

---

## Task Mapping (tasks.md items)

| #   | Description                         | Phase                                                                 |
| --- | ----------------------------------- | --------------------------------------------------------------------- |
| 29  | Add repo as workspace               | Phase 2 (already works via CLI, now adding native host inline create) |
| 30  | Remove workspace (non-destructive)  | Phase 1 + 3                                                           |
| 31  | Auto-discover repos under root dir  | Deferred (P2)                                                         |
| 37  | Non-git directory as workspace      | Deferred (P2)                                                         |
| 39  | Workspace icon/color label          | Deferred (P3)                                                         |
| —   | Delete workspace from sidebar       | Phase 3                                                               |
| —   | Create workspace with inline prompt | Phase 2                                                               |
| —   | Orphaned worktree detection         | Phase 1                                                               |
