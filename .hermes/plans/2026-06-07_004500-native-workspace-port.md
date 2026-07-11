# Native Workspace & Worktree Port Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Port thegn's workspace and worktree loading + navigation logic into the native `thegn` substrate.
**Architecture:** Make `thegn` session loading DB-aware, hydrate the sidebar with actual known workspaces, build real worktree tabs per workspace, and wire native command actions to navigate them.
**Tech Stack:** Rust, `thegn-core::db`, `thegn-host` native components.

---

### Task 1: Native workspace loading & sidebar population

**Objective:** On startup, read the DB's registered workspaces (repos) and populate the sidebar with them, replacing the dummy "hydrating..." text.

**Files:**

- Modify: `crates/thegn-host/src/run.rs` (hydration logic)
- Modify: `crates/thegn-host/src/chrome.rs` (FrameModel & sidebar rendering)

**Step 1: Write failing test**
In `crates/thegn-host/src/run.rs`:

```rust
#[test]
fn hydration_worker_loads_real_workspaces_into_sidebar() {
    // Requires a mock DB or test setup that proves `build_model` uses `db.workspaces()`
    // instead of `db.recent_repos()` for the sidebar list.
}
```

**Step 2: Run test to verify failure**
Run: `cargo test -p thegn-host hydration_worker_loads_real_workspaces_into_sidebar`
Expected: FAIL.

**Step 3: Write minimal implementation**
In `crates/thegn-host/src/run.rs::build_model`:
Replace the `recent_repos` call with `db.workspaces()`.
Update `FrameModel` to store `WorkspaceRow` or the necessary data to render them.

**Step 4: Run test to verify pass**
Run: `cargo test -p thegn-host hydration_worker_loads_real_workspaces_into_sidebar`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/thegn-host/src/run.rs crates/thegn-host/src/chrome.rs
git commit -m "feat(host): populate sidebar with native workspaces from DB"
```

---

### Task 2: Native session reconstruction from DB

**Objective:** The `thegn` session resurrect path currently creates a dummy `{cwd}/home` tab. It needs to read the DB `tab_layout` table for the target session (which represents the workspace), defaulting to creating a `{slug}/home` tab if the DB is empty for that repo.

**Files:**

- Modify: `crates/thegn-host/src/run.rs` (`load_or_seed_session`)
- Modify: `crates/thegn-core/src/db.rs` (if new queries are needed, but `tabs_for_session` exists)

**Step 1: Write failing test**
In `crates/thegn-host/src/run.rs`:

```rust
#[test]
fn load_or_seed_session_recovers_tabs_from_db_when_present() {
    // Setup test DB with `tab_layout` rows.
    // Call load_or_seed_session.
    // Assert tabs match DB, not just a dummy cwd/home.
}
```

**Step 2: Run test to verify failure**
Run: `cargo test -p thegn-host load_or_seed_session_recovers_tabs_from_db_when_present`
Expected: FAIL.

**Step 3: Write minimal implementation**
Update `load_or_seed_session` to correctly identify the workspace session name (likely the `repo_path` or `slug`), call `db.tabs_for_session()`, and map them to `Tab`s. If empty, fallback to the current behavior but properly scoped to the repo slug.

**Step 4: Run test to verify pass**
Run: `cargo test -p thegn-host load_or_seed_session_recovers_tabs_from_db_when_present`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/thegn-host/src/run.rs
git commit -m "feat(host): reconstruct native session tabs from DB tab_layout"
```

---

### Task 3: Wire native workspace switching action

**Objective:** When the user selects a workspace in the palette or sidebar, `thegn` needs to swap out the active session's tabs for the new workspace's tabs, persist the old ones, and redraw.

**Files:**

- Modify: `crates/thegn-host/src/run.rs` (action dispatch)
- Modify: `crates/thegn-host/src/keymap.rs` (if new action needed)

**Step 1: Write failing test**
In `crates/thegn-host/src/run.rs`:

```rust
#[test]
fn action_switch_workspace_reloads_tabs_for_new_target() {
    // Dispatch Action::SwitchWorkspace.
    // Assert session tabs are swapped to the new repo.
}
```

**Step 2: Run test to verify failure**
Run: `cargo test -p thegn-host action_switch_workspace_reloads_tabs_for_new_target`
Expected: FAIL.

**Step 3: Write minimal implementation**
In `run.rs` event loop, handle `Action::SwitchWorkspace` (or similar).

1. Persist current session state.
2. Load new session state via `load_or_seed_session`.
3. Set `need_relayout = true`, `dirty = true`.

**Step 4: Run test to verify pass**
Run: `cargo test -p thegn-host action_switch_workspace_reloads_tabs_for_new_target`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/thegn-host/src/run.rs
git commit -m "feat(host): implement native workspace switching"
```

---

### Task 4: Wire native worktree creation and switching

**Objective:** Implement native actions for `NewWorktree` and tab navigation so that adding a worktree creates a new native tab and `tab:*` actions focus them.

**Files:**

- Modify: `crates/thegn-host/src/run.rs`

**Step 1: Write failing test**
In `crates/thegn-host/src/run.rs`:

```rust
#[test]
fn action_new_worktree_adds_tab_and_focuses_it() {
    // Dispatch Action::NewWorktree
    // Assert tab count increases and active tab points to new tab.
}
```

**Step 2: Run test to verify failure**
Run: `cargo test -p thegn-host action_new_worktree_adds_tab_and_focuses_it`
Expected: FAIL.

**Step 3: Write minimal implementation**
In `run.rs` event loop, implement `Action::NewWorktree` to add a new `Tab` to `session.tabs` and set `session.active` to it, then trigger a redraw. Wire `Action::GotoTab(name)` (if it doesn't exist, create it) to switch to the specified tab.

**Step 4: Run test to verify pass**
Run: `cargo test -p thegn-host action_new_worktree_adds_tab_and_focuses_it`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/thegn-host/src/run.rs
git commit -m "feat(host): wire native worktree creation and tab navigation"
```
