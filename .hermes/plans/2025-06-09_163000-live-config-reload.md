# Live Config Reload Implementation Plan

> **For Hermes:** Use subagent-driven_development skill to implement this plan task-by-task.

**Goal:** Add live config reload to `szhost` (the native terminal host in `crates/superzej-host`) so changes to `~/.config/superzej/config.toml` are picked up without restarting.

**Architecture:** Background thread watches the config file via `notify`; signals the main event loop via an mpsc channel; event loop reloads config and rebuilds the keymap.

**Tech Stack:** `notify` crate (already in workspace), `std::thread`, `std::sync::mpsc`.

---

## Task 1: Add `notify` dependency to superzej-host

**Objective:** Enable `notify` crate usage in the host binary.

**Files:**
- Modify: `crates/superzej-host/Cargo.toml`

**Step 1: Add notify dependency**

Add to `[dependencies]`:
```toml
notify = { workspace = true, default-features = false, features = ["macos_kqueue"] }
```

Note: Workspace default features might include `serde`, which we don't need here, so we disable defaults and explicitly enable the kqueue feature for macOS support if needed, though the default should work on Linux. Let's check if the workspace has a `notify` definition.
Actually, let's just add it as `notify = "6"` to keep it simple, matching `superzej-cli`.

```toml
notify = "6"
```

**Step 2: Verify build**

Run: `cargo build -p superzej-host`
Expected: SUCCESS

---

## Task 2: Create config watcher background thread

**Objective:** Spawn a thread that watches the config file and signals changes.

**Files:**
- Modify: `crates/superzej-host/src/run.rs`

**Step 1: Add imports and channel**

Add to imports at top of `run.rs`:
```rust
use notify::{recommended_watcher, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc::{channel as mpsc_channel, Sender as MpscSender};
```

In `run()` function (around line 614-615, before `event_loop` call), add:
```rust
let (config_tx, config_rx) = mpsc_channel::<superzej_core::config::Config>();

// Spawn config watcher thread
let config_path = superzej_core::config::Config::path();
std::thread::spawn(move || {
    if let Some(parent) = config_path.parent() {
        if let Ok(mut watcher) = recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(ev) = res {
                // Reload on Modify, Create, or Rename (file renamed/deleted might also trigger rename/remove)
                if matches!(ev.kind, 
                    notify::EventKind::Modify(_) | 
                    notify::EventKind::Create(_) | 
                    notify::EventKind::Remove(_)
                ) {
                    // Attempt to reload config
                    let new_cfg = superzej_core::config::Config::load_layered(
                        &superzej_core::config::ProcessEnv, 
                        None, 
                        None
                    );
                    let _ = config_tx.send(new_cfg);
                }
            }
        }) {
            // Watch the parent directory to catch renames/deletes of config.toml
            let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
            
            // Keep watcher alive - block forever (or use a join handle if we wanted graceful shutdown, but for now simple is fine)
            loop { std::thread::sleep(std::time::Duration::MAX); }
        }
    }
});
```

Wait, `Config::load_layered` takes `&dyn EnvSource`. `ProcessEnv` is a unit struct. But `ProcessEnv` is defined in `superzej_core::config`. We need to import it.

Wait, `Config::load_layered` takes references to env source. We can't easily move it to a thread without cloning or referencing. We need to clone `ProcessEnv` or use a `'static` lifetime.
Actually, `ProcessEnv` is a zero-sized type (unit struct) or simple struct. Let's check.
```rust
pub struct ProcessEnv;
impl EnvSource for ProcessEnv { ... }
```
Yes, `ProcessEnv` can be cloned or created in the thread.

Wait, I need to check if `config_tx` needs to be `MpscSender<Config>` or if we can just send a "reload" signal and let the main loop reload. Sending the `Config` is cleaner because the watcher thread can handle the parsing/loading errors and just send the result. Or simpler: just send a signal, and let main loop reload. Let's do the latter for simplicity (less data in channel).

Revised Plan for Task 2:
1. Channel: `let (config_reload_tx, config_reload_rx) = mpsc_channel::<()>();`
2. Thread: Spawns, watches, sends `()` when change detected.
3. Main loop: Polls `config_reload_rx`.

Actually, `recommended_watcher` needs `Send` to be used in a thread? It's usually fine.
The config file path: `Config::path()` returns `PathBuf`.
We need to make sure we import `superzej_core::config::ProcessEnv` in `run.rs` if we want to use it inside the thread, or just re-create it.

Let's refine the thread code:
```rust
std::thread::spawn(move || {
    let env = superzej_core::config::ProcessEnv;
    let config_path = superzej_core::config::Config::path();
    // ... watcher setup ...
});
```

Wait, we also need to handle the case where the config file doesn't exist initially. `Config::load_layered` handles that (returns default).

Wait, if we just watch the parent directory, we might get spurious events. But that's fine, we just reload.

Wait, `Config::path()` returns the full path to `config.toml`. If we watch the parent, we get events for any file in the config dir. We should filter by `config_path.file_name()`. But simpler: just reload on any event in that dir.

Wait, we need to make sure `config_rx` is passed to `event_loop`.

**Step 2: Pass channel to event_loop**

Modify function signature of `event_loop` (line 829):
```rust
fn event_loop<T: Terminal>(
    // ... existing params ...
    config_rx: std::sync::mpsc::Receiver<()>, // NEW
) -> Result<()> {
```

Update call site (line 618):
```rust
event_loop(
    &mut buf, session, model, model_tx, model_rx, rows, cols, keymap, mode,
    config_rx, // NEW
)?;
```

---

## Task 3: Handle config reload in event loop

**Objective:** When receiving a reload signal, re-initialize the keymap.

**Files:**
- Modify: `crates/superzej-host/src/run.rs`

**Step 1: Poll channel in loop**

In `event_loop`, inside the main `loop { ... }`, after the `model_rx` polling (around line 956), add:

```rust
// Check for config reload
if let Ok(()) = config_rx.try_recv() {
    let new_cfg = superzej_core::config::Config::load_layered(
        &superzej_core::config::ProcessEnv, 
        None, 
        None
    );
    keymap = crate::keymap::default_keymap_with_config(&new_cfg);
    // Optionally update status message
    model.status = "Config reloaded".into();
    dirty = true;
}
```

Wait, we also need to import `ProcessEnv` in `run.rs` scope or use fully qualified path. We used fully qualified path in thread, we can use it here too.

Wait, what about error handling? If `Config::load_layered` fails (e.g. parse error), it warns and returns default. So we are safe.

**Step 2: Verify build**

Run: `cargo build -p superzej-host`
Expected: SUCCESS

---

## Task 4: Add debouncing (Optional/Polish)

**Objective:** Prevent rapid reloading when saving config file repeatedly (e.g. text editor auto-save).

**Files:**
- Modify: `crates/superzej-host/src/run.rs`

The `notify` crate has `recommended_watcher` which is raw events. We can add a simple debounce in the channel receiver or use a timeout in the thread.

Simple debounce: In the thread, keep track of `last_sent`. If < 500ms ago, skip sending.

```rust
let mut last_send = std::time::Instant::now();
// ... inside watcher callback ...
if last_send.elapsed() > std::time::Duration::from_millis(500) {
    let _ = config_tx.send(());
    last_send = std::time::Instant::now();
}
```

Add this to the watcher callback.

---

## Task 5: Integration Test

**Objective:** Verify the reload works manually.

**Step 1: Run szhost**

Run: `cargo run -p superzej-host --bin szhost`

**Step 2: Modify config**

Edit `~/.config/superzej/config.toml` (or create if missing).
Add a custom action or keybind.

**Step 3: Verify**

Check if the new keybind works (if implemented in keymap) or if status shows "Config reloaded".

**Step 4: Cleanup**

Kill the szhost process.

---

## Summary

- **Files changed:**
  - `crates/superzej-host/Cargo.toml`
  - `crates/superzej-host/src/run.rs`

- **Risks:**
  - Thread safety: `notify` watcher is not `Send` (it is). `Config` loading is cheap enough.
  - File watcher reliability: Works on Linux/macOS.
  - Memory leaks: Thread runs forever (fine for long-lived process).

- **Tradeoffs:**
  - Simple approach (reload everything) vs granular (reload only changed sections). Current plan reloads everything, which is fine for `Config` size.
  - Blocking vs non-blocking config reload. Used non-blocking channel.
