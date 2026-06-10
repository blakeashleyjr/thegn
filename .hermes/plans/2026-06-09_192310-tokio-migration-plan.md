# Tokio Event Loop Migration Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Migrate the core native host event loop in `szhost` from a single-threaded blocking loop with `std::sync::mpsc` to a fully asynchronous `tokio` multi-tasking architecture with bounded backpressure.

**Architecture:**

1. Upgrade `run::main` and `run::event_loop` to be `async fn`.
2. Wrap synchronous legacy code (e.g. DB reads, Git calls, Github proxy calls) with `tokio::task::spawn_blocking` to prevent blocking the `tokio` runtime, avoiding the need to make `superzej-svc` completely async yet.
3. Replace the `std::sync::mpsc` unbounded channels feeding the PTY data with a bounded `tokio::sync::mpsc::channel(1024)`.
4. Update the `[String]` arguments in `panes.spawn_argv` closures to `Vec<String>` to resolve async closure bounds inference issues.

**Tech Stack:** Rust, `tokio`, `termwiz`.

---

### Task 1: Fix Cargo Edition and add Tokio feature

**Objective:** Ensure `tokio` with `full` features is enabled and `Cargo.toml` edition inheritance works.

**Files:**

- Modify: `crates/superzej-host/Cargo.toml`

**Step 1: Check build**

Run: `cargo check -p superzej-host`
Expected: PASS

**Step 2: Update `Cargo.toml`**

Replace `edition.workspace = true` with `edition = "2021"` to avoid older cargo versions failing to inherit.
Add `tokio` to dependencies if missing.

```toml
[package]
name = "superzej-host"
version.workspace = true
edition = "2021"
license.workspace = true

[dependencies]
# ... existing ...
tokio = { workspace = true, features = ["full"] }
```

**Step 3: Run check**

Run: `cargo check -p superzej-host`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/superzej-host/Cargo.toml
git commit -m "build: explicitly specify 2021 edition and tokio feature"
```

---

### Task 2: Convert `main.rs` and `run.rs` to async

**Objective:** Set up the basic `#[tokio::main]` runtime and make `run::main` and `event_loop` async.

**Files:**

- Modify: `crates/superzej-host/src/main.rs`
- Modify: `crates/superzej-host/src/run.rs`

**Step 1: Update `main.rs`**

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run::main().await
}
```

**Step 2: Update `run::main` and `run::event_loop` signatures**

In `crates/superzej-host/src/run.rs`:

```rust
// Change main
pub async fn main() -> Result<()> {
    // ...
    let result = event_loop(
        &mut buf, session, model, model_tx, model_rx, rows, cols, keymap, mode,
    ).await;
    // ...
}

// Change event_loop
#[allow(clippy::too_many_arguments)]
async fn event_loop(
    buf: &mut BufferedTerminal<impl Terminal>,
    mut session: crate::session::Session,
    mut model: FrameModel,
    model_tx: Sender<FrameModel>,
    mut model_rx: Receiver<FrameModel>,
    mut rows: usize,
    mut cols: usize,
    mut keymap: crate::keymap::KeyMap,
    mut mode: crate::keymap::Mode,
) -> Result<()> {
```

_(Note: Change `Keymap` to `KeyMap`, change `SystemTerminal` to `impl Terminal`, change `model_rx: Receiver` to `mut model_rx` if tokio requires mutability for `.recv()`.)_

**Step 3: Temporarily stub `run.rs` channel usage**

For this task, replace `std::sync::mpsc::channel` with `tokio::sync::mpsc::unbounded_channel` at the top of `run::main` and `event_loop`.
Update `model_rx.try_recv()` to `model_rx.try_recv().ok()`.

**Step 4: Fix `and_then(|argv|` type bounds**

In `Action::ToggleDrawer` and `Action::Yazi`, update the closure to explicitly take `Vec<String>`:

```rust
    .and_then(|argv: Vec<String>| {
        panes.spawn_argv(&argv, cwd.as_deref(), chrome.center).ok()
    })
```

**Step 5: Run tests**

Run: `cargo check -p superzej-host`
Expected: Failures likely related to `PaneEvent` channels which we fix in Task 3.

---

### Task 3: Migrate Channels and Thread Spawning

**Objective:** Change `spawn_model_hydration` and pane logic to use `tokio::task::spawn_blocking` and `tokio::sync::mpsc`.

**Files:**

- Modify: `crates/superzej-host/src/run.rs`
- Modify: `crates/superzej-host/src/pane.rs`

**Step 1: Update Hydration Functions**

```rust
// In run.rs
fn spawn_model_hydration(tx: tokio::sync::mpsc::UnboundedSender<FrameModel>, session: crate::session::Session) {
    tokio::task::spawn_blocking(move || {
        if let Ok(db) = superzej_core::db::Db::open() {
            let _ = tx.send(build_model(&session, &db));
        }
    });
}
```

**Step 2: Update `pane.rs` to use bounded `tokio::sync::mpsc::channel(1024)`**

```rust
// In pane.rs
use tokio::sync::mpsc::Sender;

pub struct PtyPane {
    // ...
}

impl PtyPane {
    pub fn spawn(
        // ...
        tx: Sender<PaneEvent>,
    ) -> Result<Self> {
        // ...
        tokio::task::spawn_blocking(move || {
            let mut reader = pty.try_clone_reader().unwrap();
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 { break; }
                if tx.blocking_send(PaneEvent::Data(id, buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            let _ = tx.blocking_send(PaneEvent::Exit(id));
        });
        // ...
    }
}
```

**Step 3: Update `run.rs` Panes struct**

```rust
struct Panes {
    table: std::collections::HashMap<u32, PtyPane>,
    next_id: u32,
    tx: tokio::sync::mpsc::Sender<PaneEvent>,
}
```

**Step 4: Update `event_loop` polling**

Replace the non-blocking `try_recv()` loop with a `tokio::select!` or timeout over the `tokio` channels and the `buf.terminal().poll_input()`.

```rust
// Replace drain_pty_events with direct async read
while let Ok(ev) = rx.try_recv() {
    // apply output
}
```

**Step 5: Test and Commit**

Run: `cargo test -p superzej-host`

```bash
git commit -am "perf(host): migrate event loop to tokio runtime and bounded channels"
```
