# szhost Architectural Performance Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Overhaul the `szhost` (native host) substrate by replacing the initial first-pass jank fixes with robust architectural optimizations: moving to a `tokio` multi-task split, batching terminal rendering via entire rows, bounding PTY channels with backpressure, and reducing IPC overhead for the WASM UI plugins.

**Architecture:**
The host drops the single-loop `std::sync::mpsc` design in favor of `tokio`. We split into a dedicated UI/render task and separate I/O tasks for PTY draining. We optimize `termwiz` cell blitting by changing `PaneEmulator` to yield row references instead of individual cells. We replace `serde_json` over `zellij::pipe` with a zero-copy structured or highly optimized text format.

**Tech Stack:** Rust, `tokio`, `termwiz`, `bincode`/`rkyv` (or optimized text serialization).

---

### Task 1: Migrate Event Loop to `tokio` and `tokio::sync::mpsc`

**Objective:** Split the monolithic `run` loop into a `tokio` app with a dedicated rendering task and separate PTY reader/hydration tasks.

**Files:**

- Modify: `crates/superzej-host/src/main.rs`
- Modify: `crates/superzej-host/src/run.rs`
- Modify: `crates/superzej-host/Cargo.toml`

**Step 1: Add `tokio` dependency**

```bash
cargo add tokio -p superzej-host --features full
```

**Step 2: Convert `run.rs` loop to use `tokio::sync::mpsc`**

```rust
// In `crates/superzej-host/src/run.rs`
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::time::timeout;

// Change signature to async
pub async fn run_host(...) -> Result<()> {
    let (tx, mut rx) = channel::<HostEvent>(1024);

    // Convert existing thread::spawn calls to tokio::spawn
    // Replace `rx.try_recv()` and `rx.recv_timeout()` with `tokio::select!` or `timeout(Duration, rx.recv()).await`
}
```

**Step 3: Update `main.rs` entrypoint**

```rust
// In `crates/superzej-host/src/main.rs`
#[tokio::main]
async fn main() -> Result<()> {
    // ... setup ...
    crate::run::run_host(...).await
}
```

**Step 4: Verify Compilation and Run**

Run: `cargo build -p superzej-host`
Expected: Compiles with no async/await syntax errors.

**Step 5: Commit**

```bash
git add crates/superzej-host/
git commit -m "perf(host): migrate single-thread event loop to tokio"
```

---

### Task 2: Coalescing Bounded Channels for PTY Output

**Objective:** Prevent chatty shell panes (e.g. `cat large_file`) from consuming unbounded memory or starving the render loop.

**Files:**

- Modify: `crates/superzej-host/src/pane.rs`

**Step 1: Implement bounded buffer read loops**

Modify the PTY reader thread spawned in `Pane::new()` to read chunks but drop/coalesce data if the `tokio` sender is full, acting as backpressure.

```rust
// In `crates/superzej-host/src/pane.rs`

pub fn new(...) -> Self {
    let (tx, rx) = tokio::sync::mpsc::channel(256); // Bounded channel

    tokio::task::spawn_blocking(move || {
        let mut reader = pty.try_clone_reader().unwrap();
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 { break; }

            // If the channel is full, this `.blocking_send` will yield/block,
            // naturally pacing the PTY output rather than flooding RAM.
            if tx.blocking_send(PaneEvent::Data(buf[..n].to_vec())).is_err() {
                break;
            }
        }
    });
}
```

**Step 2: Remove the old "drain budget" hack**

In `run.rs`, remove the ad-hoc chunk/budget limit since `tokio::select!` and the bounded channel handle fairness natively.

**Step 3: Test and Commit**

Run: `cargo test -p superzej-host`

```bash
git add crates/superzej-host/src/pane.rs crates/superzej-host/src/run.rs
git commit -m "perf(host): implement bounded PTY channels with backpressure"
```

---

### Task 3: Direct Row Rendering API (Compositor Cleanup)

**Objective:** Reduce the massive allocation churn in the composition layer by fetching entire rows instead of individual grid cells.

**Files:**

- Modify: `crates/superzej-host/src/emulator.rs`
- Modify: `crates/superzej-host/src/compositor.rs`

**Step 1: Add `row_text` and `row_styles` to `PaneEmulator`**

```rust
// In `crates/superzej-host/src/emulator.rs`

pub trait PaneEmulator: Send {
    // ... existing ...

    /// Borrow the underlying row text as a single string if supported
    fn row_text(&self, row: u16) -> Option<String> { None }

    /// Fallback for `row_text` implementation in `Vt100Emulator`
}
```

**Step 2: Optimize `compose_pane` in `compositor.rs`**

```rust
// In `crates/superzej-host/src/compositor.rs`
pub fn compose_pane(surface: &mut Surface, emu: &dyn PaneEmulator, rect: Rect) {
    let (erows, ecols) = emu.size();

    for row in 0..rect.rows.min(erows as usize) {
        // FAST PATH: If the emulator can give us the whole row, blit it directly
        if let Some(text) = emu.row_text(row as u16) {
            surface.add_change(Change::CursorPosition {
                x: Position::Absolute(rect.x),
                y: Position::Absolute(rect.y + row),
            });
            surface.add_change(Change::Text(text));
            continue;
        }

        // SLOW PATH: Fallback to existing cell-by-cell iteration
        // ... existing cell loop ...
    }
}
```

**Step 3: Test and Commit**

Run: `cargo test -p superzej-host` (Ensure `composing_a_grid_reproduces_its_text` passes).

```bash
git add crates/superzej-host/src/emulator.rs crates/superzej-host/src/compositor.rs
git commit -m "perf(host): implement fast-path row blitting in compositor"
```

---

### Task 4: Reduce JSON Parsing Overhead in WASM Pipes

**Objective:** Reduce the `serde_json` deserialization cost inside the WASM `panel` plugin when it receives `superzej_diff` and `superzej_pr` pipes.

**Files:**

- Modify: `plugin/panel/src/main.rs`
- Modify: `crates/superzej-cli/src/commands/watch.rs`

**Step 1: Serialize diffs as optimized TSV payloads**

Instead of building a JSON map `{"worktree": wt, "files": tsv}`, serialize the pipe message cleanly so the WASM side doesn't have to invoke `serde_json::from_str`.

```rust
// In `crates/superzej-cli/src/commands/watch.rs`
fn push_diff(url: &str, wt: &str) {
    let tsv = diff::files_for(Path::new(wt));
    // ... DB cache logic ...

    // Pipe payload: purely text, formatted as "worktree_path\n<TSV DATA>"
    let payload = format!("{}\n{}", wt, tsv);
    zellij::pipe_plugin(url, "superzej_diff", &payload);
}
```

**Step 2: Fast-parse in the Plugin**

```rust
// In `plugin/panel/src/main.rs`
            "superzej_diff" => {
                if let Some(payload) = pipe.payload {
                    // Optimized manual parse: zero JSON allocation
                    if let Some((wt, files_tsv)) = payload.split_once('\n') {
                        let parsed = parse_files(files_tsv);
                        self.diff_cache.insert(wt.to_string(), parsed.clone());
                        if Some(wt) == self.worktree.as_deref() {
                            self.files = parsed;
                            return true;
                        }
                    }
                }
                false
            }
```

**Step 3: Build Plugins and Commit**

Run: `just build-plugins`

```bash
git add plugin/panel/src/main.rs crates/superzej-cli/src/commands/watch.rs
git commit -m "perf(panel): drop JSON serialization for diff pipes to reduce WASM CPU cost"
```
