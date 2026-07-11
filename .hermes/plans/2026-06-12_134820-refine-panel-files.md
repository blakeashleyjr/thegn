# Thegn Feature Implementation: Refine Panel Files/Changes Section Interaction

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Make the files and changes lists in the thegn panel faster to navigate, default to expanded, and unify editor commands (bat, external editor, terminal editor).

**Architecture:** The modifications exist entirely within `crates/thegn-host/src/run.rs` (the main event loop), `crates/thegn-host/src/hydrate.rs`, and `crates/thegn-host/src/panel/`. The underlying data model (the tree structure from `build_file_tree`) is largely untouched, but the default UI state is adjusted so that the Changes section (which contains files) is expanded by default. The keybindings in `run.rs` are extended and unified to support `Enter` (bat), `shift+o` / `O` (terminal editor in new pane), and `ctrl+o` (external editor).

**Tech Stack:** Rust, thegn-host event loop.

---

### Task 1: Revert WIP dirty state in run.rs and prepare for clean changes

**Objective:** The previous agent session left `run.rs` in a broken state due to failed patch attempts parsing Rust 2024 features on a 2015 edition fallback. We must revert the uncommitted changes to `run.rs` to start from a clean slate.

**Files:**

- Modify: `crates/thegn-host/src/run.rs`

**Step 1: Check out clean run.rs**

```bash
git checkout -- crates/thegn-host/src/run.rs
cargo check -p thegn-host
```

_Expected: Clean compilation._

**Step 2: Commit cleanup**

```bash
git commit -m "chore: revert broken run.rs experiments"
```

_(Skip if `git diff` shows no other changes you want to keep)_

### Task 2: Unify keybindings for Files and Changes sections

**Objective:** Ensure that `Enter` opens in `bat`, `Shift+O` (`O`) opens in the terminal editor (new pane), and `Ctrl+O` opens in an external editor.

**Files:**

- Modify: `crates/thegn-host/src/run.rs` (in `event_loop`, `PanelMsg::Select` and direct key handling for `Section::Files`/`Section::Changes`)

**Step 1: Update key dispatch in event loop**

Locate the key handling block in `event_loop` for `Section::Files` and `Section::Changes`. Update the match arms. Note that `PanelMsg::Select` (triggered by Enter via `accordion_key`) should map to the `bat` command.

```rust
// Inside run.rs event_loop, locate the `PanelMsg::Select => match panel_ui.open { ... }` block
// and update `Section::Files` and `Section::Changes` to use `bat`.
// Also locate the direct key handlers for `KeyCode::Char('o')`, `KeyCode::Char('O')`, and `Ctrl-O`.
```

_Implementation detail for `bat` (Enter / PanelMsg::Select):_

```rust
Section::Files | Section::Changes => {
    let path = if panel_ui.open == Section::Files {
        changed_file_at(&model, panel_ui.cursor)
    } else {
        panel_ui.chg_sel.or(Some(panel_ui.cursor))
            .and_then(|i| model.panel.changes.get(i))
            .map(|c| c.path.clone())
    };
    if let Some(path) = path {
        let bat = keymap.config().tool_command("bat").unwrap_or("bat --paging=always").to_string();
        let cmd = format!("{bat} {}", test_shell_quote(&path));
        let cwd = active_cwd(&session);
        // Using open_command_pane to open in center window
        open_command_pane(&mut session, &mut panes, focused, &cmd, cwd.as_deref(), chrome.center);
        focus.zone = crate::focus::Zone::Center;
        refresh_tab_model(&mut model, &session, &mut sb);
        need_relayout = true;
    }
}
```

_Implementation detail for `O` (Shift+O) - Terminal Editor in new pane:_

```rust
(Section::Files, KeyCode::Char('O')) | (Section::Changes, KeyCode::Char('O')) => {
    // ... resolve path ...
    if let Some(path) = path {
        let cmd = editor_open_command(keymap.config(), &path, None);
        let cwd = active_cwd(&session);
        open_command_pane(&mut session, &mut panes, focused, &cmd, cwd.as_deref(), chrome.center);
        focus.zone = crate::focus::Zone::Center;
        refresh_tab_model(&mut model, &session, &mut sb);
        need_relayout = true;
    }
    true
}
```

_Implementation detail for `Ctrl-O` (`\x0F`) - External Editor:_

```rust
(Section::Files, KeyCode::Char('\x0F')) | (Section::Changes, KeyCode::Char('\x0F')) => {
    // ... resolve path ...
    if let Some(path) = path {
        let wt = active_tab_path(&session);
        let abs_path = wt.join(path);
        let cmd = editor_open_command(keymap.config(), &abs_path.to_string_lossy(), None);
        // Spawn detached external process
        let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
    }
    true
}
```

**Step 2: Run test to verify compilation**
Run: `cargo test -p thegn-host`
Expected: PASS

**Step 3: Commit**

```bash
git add crates/thegn-host/src/run.rs
git commit -m "feat(panel): unify file open keybinds (Enter=bat, O=pane, C-o=external)"
```

### Task 3: Expand the first item (Changes) by default and persist state

**Objective:** The user requested "Changes (or what ever is the first item in the list) should be expanded by default" and "accordianed by default (remember last state)". `PanelUi` initialization needs to default to an expanded state, or ensure `width` is initialized to `Half` (expanded) rather than `Normal`.

**Files:**

- Modify: `crates/thegn-host/src/run.rs` (near `let mut panel_ui = crate::panel::PanelUi::default();`)
- Modify: `crates/thegn-host/src/panel/mod.rs` (if `PanelWidth::default()` needs tweaking, though usually it's handled in `run.rs` hydration).

**Step 1: Inspect `PanelUi` persistence in `run.rs`**
Currently, `run.rs` reads `ui_state_in_scope("panel")`. We need to ensure that if no width state is found, it defaults to `PanelWidth::Half` instead of `PanelWidth::Normal`, OR if the width is `Normal`, it forces the section list to behave correctly. The prompt says "accordianed by default" which implies the tree-view inside the section is expanded.

For the `Changes` section, the tree expansion is managed by the `FileEntry` structure or the list rendering. If the user means the _panel itself_ should be expanded (width), we adjust the default width.

```rust
// run.rs
let mut panel_ui = crate::panel::PanelUi::default();
// Ensure default width is expanded if no DB state exists
panel_ui.width = crate::layout::PanelWidth::Half;
if let Ok(db) = thegn_core::db::Db::open() {
    // ... db loading overrides this if present
}
```

**Step 2: Verify File Tree logic**
Ensure `crates/thegn-host/src/panel/mod.rs` or `misc.rs` where the tree is rendered doesn't collapse dirs by default. The current `build_file_tree` doesn't seem to track collapsed/expanded state per directory, meaning the whole list _is_ always expanded. If we need to remember the state, we might need a `HashSet<String>` of collapsed paths in `PanelUi`.

_Note for implementer: Check if dir collapsing is already implemented. If not, implementing tree collapsing is out of scope for a quick fix unless specifically requested, but the prompt says "accordianed by default (remember last state)". If the panel sections themselves are the accordions, they remember their state via `ui_state_in_scope("panel")` which saves the `open` section._

**Step 3: Run test**
Run: `cargo test -p thegn-host`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/thegn-host/src/run.rs
git commit -m "feat(panel): default panel to expanded width"
```

### Task 4: Ensure file list performance is "insanely fast"

**Objective:** The user noted "The files tool used to have a list of ALL files, not just changes... Make it insanely fast and efficent."

**Files:**

- Inspect: `crates/thegn-host/src/hydrate.rs` (where `git diff_files` and `git status` are called).

**Step 1: Review Hydration for bottlenecks**
`thegn-host` runs `git.status()` and `git.diff_files()` in `hydrate.rs`. If the user wants the "ALL files" list back, they are referring to `Section::Files`. Currently `Section::Files` might be rendering the same diff/status as Changes, or it used to use `git ls-files`.

In `hydrate.rs`, lines 749-756:

```rust
    if hints.open == crate::panel::Section::Files
        && let Ok(out) = loc.git_command(&["ls-files"]).output()
        && out.status.success()
    {
        panel.file_count = Some(out.stdout.iter().filter(|&&b| b == b'\n').count() as u64);
        // Does it populate panel.files here? Currently it only sets file_count.
    }
```

If we need `Section::Files` to show ALL files, we must parse `ls-files` into `panel.files`. However, parsing 50k files every 2 seconds is slow.

_Plan for implementer:_

1. Read the exact intent. The user says: "The files tool used to have a list of ALL files, not just changes (as we already have that). Super fast, easy to navigate in a small space, accordianed by default (remember last state)."
2. Modify `hydrate.rs` to fetch `git ls-files` and populate a new list or replace `panel.files` if `hints.open == Section::Files`.
3. To make it "insanely fast", use `git ls-files -z` and parse it efficiently, but only if the user expands that section.
4. Add directory collapse state (`HashSet<String>`) to `PanelUi` to support "accordianed... remember last state".

_Since Task 4 requires non-trivial state additions (tree expansion state) and `ls-files` parsing, implement Tasks 1-3 first, then evaluate the diff complexity for Task 4._
