# Instantaneous Git Diff Panel Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Eliminate the 50–150ms delay and visual flashing when switching worktree tabs in the right panel, making the diff render instantaneously (0ms lag).

**Architecture:** Move the SQLite cache lookup out of the critical tab-switch rendering path. The panel plugin will pre-fetch all session worktrees' diff/PR caches into a WASM memory `HashMap` via a new `superzej panel-snapshot --all` command on startup. When switching tabs, `refocus()` will query this local memory cache for an instant 0ms first-paint, while still triggering the background `watch` daemon hydration.

**Tech Stack:** Rust, clap (CLI), Zellij WASM plugin (panel).

---

### Task 1: Add `--all` flag to `panel-snapshot` CLI

**Objective:** Extend `superzej panel-snapshot` to dump the cache for all worktrees in a session.

**Files:**
- Modify: `crates/superzej-cli/src/commands/snapshot.rs`
- Modify: `crates/superzej-cli/src/cli.rs`

**Step 1: Write failing test / behavior check**

Run: `cargo run -p superzej-cli -- panel-snapshot --session default --all`
Expected: FAIL — error: unexpected argument `--all`

**Step 2: Add `--all` argument to `cli.rs`**

```rust
// In `crates/superzej-cli/src/cli.rs` under `pub enum Command`:
    #[command(name = "panel-snapshot", hide = true)]
    PanelSnapshot {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        tab: Option<String>,
        #[arg(long, help = "Dump cache for all session worktrees")]
        all: bool,
    },
```

Pass the `all` parameter down to `commands::snapshot::run` in `crates/superzej-cli/src/main.rs`.

**Step 3: Implement `--all` logic in `snapshot.rs`**

```rust
// In `crates/superzej-cli/src/commands/snapshot.rs`

pub fn run(session: Option<String>, tab: Option<String>, all: bool) -> Result<()> {
    let session = session.unwrap_or_else(db::session);

    if all {
        let mut results = Vec::new();
        if let Ok(db) = Db::open() {
            if let Ok(wts) = db.worktrees() {
                for w in wts.into_iter().filter(|w| w.session_name == session) {
                    let mut obj = Map::new();
                    obj.insert("tab".into(), json!(w.tab_name));
                    obj.insert("worktree".into(), json!(w.worktree));
                    
                    if let Ok(Some((pr_json, _))) = db.get_pr_cache(&w.worktree) {
                        if let Ok(v) = serde_json::from_str::<Value>(&pr_json) {
                            obj.insert("pr".into(), v);
                        }
                    }
                    if let Ok(Some((files, _))) = db.get_diff_cache(&w.worktree) {
                        obj.insert("files".into(), json!(files));
                    }
                    results.push(Value::Object(obj));
                }
            }
        }
        crate::outln!("{}", Value::Array(results));
        return Ok(());
    }

    // ... existing single-tab logic ...
```

**Step 4: Run test to verify pass**

Run: `cargo run -p superzej-cli -- panel-snapshot --session default --all`
Expected: JSON array output.

**Step 5: Commit**

```bash
git add crates/superzej-cli/src/cli.rs crates/superzej-cli/src/main.rs crates/superzej-cli/src/commands/snapshot.rs
git commit -m "feat(cli): add --all flag to panel-snapshot to bulk dump caches"
```

---

### Task 2: Maintain Memory Cache in Panel Plugin

**Objective:** Add `HashMap` caches to the panel plugin's `State` and update them when pipe messages arrive.

**Files:**
- Modify: `plugin/panel/src/main.rs`

**Step 1: Add cache structures to `State`**

```rust
// In `plugin/panel/src/main.rs`

use std::collections::HashMap;

#[derive(Default)]
struct State {
    // ... existing fields ...
    
    /// tab_name -> worktree path
    tab_to_worktree: HashMap<String, String>,
    /// worktree -> files
    diff_cache: HashMap<String, Vec<FileEntry>>,
    /// worktree -> pr JSON
    pr_cache: HashMap<String, serde_json::Value>,
    
    /// Track if we have already preloaded the cache
    preloaded: bool,
}
```

**Step 2: Update pipe handlers to keep the cache hot**

```rust
// Inside `impl ZellijPlugin for State { fn pipe(&mut self, pipe: PipeMessage) -> bool {`

            "superzej_pr" => {
                if let Some(payload) = pipe.payload {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
                        if let Some(wt) = v.get("worktree").and_then(|w| w.as_str()) {
                            self.pr_cache.insert(wt.to_string(), v.clone());
                            if Some(wt) == self.worktree.as_deref() {
                                self.pr = Some(v);
                                return true;
                            }
                        }
                    }
                }
                false
            }
            "superzej_diff" => {
                if let Some(payload) = pipe.payload {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
                        if let Some(wt) = v.get("worktree").and_then(|w| w.as_str()) {
                            if let Some(files) = v.get("files").and_then(|f| f.as_str()) {
                                let parsed = parse_files(files);
                                self.diff_cache.insert(wt.to_string(), parsed.clone());
                                if Some(wt) == self.worktree.as_deref() {
                                    self.files = parsed;
                                    return true;
                                }
                            }
                        }
                    }
                }
                false
            }
```

**Step 3: Commit**

```bash
git add plugin/panel/src/main.rs
git commit -m "feat(panel): track diff and pr cache in plugin memory"
```

---

### Task 3: Trigger and Parse `--all` Preload

**Objective:** Fetch the bulk snapshot on plugin load and parse it into the caches.

**Files:**
- Modify: `plugin/panel/src/main.rs`

**Step 1: Trigger the preload in `update`**

```rust
// Inside `impl ZellijPlugin for State { fn update(&mut self, event: Event) -> bool {`
// Inside `Event::TabUpdate(tab_info) => {`

        if !self.preloaded && self.session.is_some() {
            self.preloaded = true;
            let mut ctx = BTreeMap::new();
            ctx.insert("cmd".to_string(), "snapshot-all".to_string());
            run_command(
                &[
                    "superzej",
                    "panel-snapshot",
                    "--all",
                    "--session",
                    self.session.as_deref().unwrap_or_default(),
                ],
                ctx,
            );
        }
```

**Step 2: Parse the preload in `on_result`**

```rust
// Inside `impl State { fn on_result(&mut self, cmd: Option<&str>, stdout: Vec<u8>) -> bool {`

            Some("snapshot-all") => {
                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(text.trim()) {
                    for v in arr {
                        if let (Some(tab), Some(wt)) = (
                            v.get("tab").and_then(|t| t.as_str()),
                            v.get("worktree").and_then(|w| w.as_str()),
                        ) {
                            self.tab_to_worktree.insert(tab.to_string(), wt.to_string());
                            
                            if let Some(files) = v.get("files").and_then(|f| f.as_str()) {
                                self.diff_cache.insert(wt.to_string(), parse_files(files));
                            }
                            if let Some(pr) = v.get("pr") {
                                if !pr.is_null() {
                                    self.pr_cache.insert(wt.to_string(), pr.clone());
                                }
                            }
                        }
                    }
                    // Try an immediate refocus in case the active tab matches now
                    return self.refocus_instant();
                }
                false
            }
```

(Note: we'll build `refocus_instant` in the next task).

**Step 3: Commit**

```bash
git add plugin/panel/src/main.rs
git commit -m "feat(panel): preload all session caches on startup"
```

---

### Task 4: Zero-latency Tab Switching via Local Cache

**Objective:** Wire up `refocus()` to look at the memory cache for a 0ms first-paint, falling back to a background `panel-snapshot` query to handle newly-created tabs.

**Files:**
- Modify: `plugin/panel/src/main.rs`

**Step 1: Write `refocus_instant` and modify `refocus`**

```rust
// Strip the ` ·N` page suffix to map multi-pane tabs to their root worktree tab
fn base_tab_name(tab: &str) -> &str {
    match tab.find(" \u{b7}") {
        Some(idx) => &tab[..idx],
        None => tab,
    }
}

// Inside `impl State {`

    fn refocus_instant(&mut self) -> bool {
        let Some(t) = self.active_tab.as_deref() else { return false };
        let base = base_tab_name(t);
        
        if let Some(wt) = self.tab_to_worktree.get(base).cloned() {
            self.worktree = Some(wt.clone());
            if let Some(files) = self.diff_cache.get(&wt) {
                self.files = files.clone();
            } else {
                self.files.clear();
            }
            self.pr = self.pr_cache.get(&wt).cloned();
            return true; // We changed state, request a render
        }
        false
    }

    fn refocus(&mut self) -> bool {
        let (Some(s), Some(t)) = (self.session.clone(), self.active_tab.clone()) else {
            return false;
        };
        let id = (s.clone(), t.clone());
        if self.identity.as_ref() == Some(&id) {
            return false;
        }
        self.identity = Some(id);
        
        // 1. Attempt a 0ms instantaneous paint from local memory cache
        let mut changed = self.refocus_instant();
        
        if self.worktree.is_none() {
            // Fallback clear if we have absolutely nothing
            self.files.clear();
            self.pr = None;
            changed = true;
        }

        // 2. STILL run single-tab panel-snapshot in the background.
        // This ensures newly created tabs get resolved properly, and triggers
        // the watch daemon to focus the new worktree path.
        let mut ctx = BTreeMap::new();
        ctx.insert("cmd".to_string(), "snapshot".to_string());
        run_command(
            &["superzej", "panel-snapshot", "--session", &s, "--tab", &t],
            ctx,
        );
        
        changed
    }
```

**Step 2: Update `on_result("snapshot")` to populate caches too**

```rust
            Some("snapshot") => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) {
                    let new_wt = v.get("worktree").and_then(|w| w.as_str()).map(String::from);
                    
                    if let Some(wt) = new_wt.clone() {
                        let base = base_tab_name(self.active_tab.as_deref().unwrap_or_default());
                        self.tab_to_worktree.insert(base.to_string(), wt.clone());
                        
                        if let Some(files) = v.get("files").and_then(|f| f.as_str()) {
                            self.diff_cache.insert(wt.clone(), parse_files(files));
                        }
                        if let Some(pr) = v.get("pr") {
                            if !pr.is_null() {
                                self.pr_cache.insert(wt.clone(), pr.clone());
                            }
                        }
                    }

                    // Apply to current view
                    self.worktree = new_wt;
                    // ... existing hydration logic ...
```

**Step 3: Run plugin build to verify correctness**

Run: `just build-plugins`
Expected: compiles successfully.

**Step 4: Commit**

```bash
git add plugin/panel/src/main.rs
git commit -m "feat(panel): instant 0ms tab switching via local diff cache"
```
