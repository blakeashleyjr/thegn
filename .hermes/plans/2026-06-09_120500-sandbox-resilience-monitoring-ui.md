# Sandbox Resilience, Monitoring, and UI Integration Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Complete the sandbox architecture by adding an orphan garbage collector, a resource utilization API, and storing the resolved sandbox backend in SQLite for WASM plugin UI representation.

**Architecture:**

1. **Frontend Integration:** Store the `sandbox_backend` per worktree in the SQLite database so UI components (like the status bar) can render the active backend (🐳, 📦, etc.).
2. **Resource Monitoring API:** Parse `podman stats`/`docker stats` to surface the CPU and Memory metrics of active sandboxes. We will add a `stats()` function to `crates/thegn-core/src/sandbox.rs` and plumb it into `crates/thegn-cli/src/commands/stats.rs`.
3. **Orphan Garbage Collection:** OCI persistent containers (`thegn-*`) can be leaked if a user bypasses the Thegn CLI (e.g., `git worktree remove` manually). We will add a `sandbox::run_gc()` routine and invoke it asynchronously on startup in `crates/thegn-cli/src/commands/launch.rs` (the host daemon entrypoint).

**Tech Stack:** Rust, SQLite, Podman/Docker CLI.

---

### Task 1: Store Sandbox Backend in Database

**Objective:** Extend the database schema to track the resolved sandbox backend and write it during `pick_agent`.

**Files:**

- Modify: `crates/thegn-core/src/db.rs`
- Modify: `crates/thegn-cli/src/commands/pick_agent.rs`

**Step 1: Write failing DB schema test**

```rust
// In crates/thegn-core/src/db.rs -> mod tests { ... }
#[test]
fn test_db_stores_sandbox_backend() {
    let db = Db::open_memory().unwrap();
    db.put_worktree("tab", "/repo", "/wt", "main", None).unwrap();
    db.set_worktree_sandbox("/wt", "podman").unwrap();
    let sb = db.worktree_sandbox("/wt").unwrap();
    assert_eq!(sb, Some("podman".to_string()));
}
```

**Step 2: Run test to verify failure**

**Step 3: Write minimal implementation**

```rust
// In crates/thegn-core/src/db.rs
// 1. In `ensure_tables`, alter `worktrees` table to add `sandbox_backend TEXT`:
//    Since modifying existing deployed schemas without full migrations is risky,
//    add a conditional ALTER TABLE:
pub fn ensure_tables(conn: &Connection) -> Result<()> {
    // ... existing creates ...
    let _ = conn.execute("ALTER TABLE worktrees ADD COLUMN sandbox_backend TEXT", params![]);
    // ...
}

// 2. Add methods:
pub fn set_worktree_sandbox(&self, wt: &str, backend: &str) -> Result<()> {
    self.conn.execute(
        "UPDATE worktrees SET sandbox_backend=?2 WHERE worktree=?1",
        params![wt, backend],
    )?;
    Ok(())
}

pub fn worktree_sandbox(&self, wt: &str) -> Result<Option<String>> {
    let mut stmt = self.conn.prepare("SELECT sandbox_backend FROM worktrees WHERE worktree=?1")?;
    let mut rows = stmt.query(params![wt])?;
    if let Some(row) = rows.next()? {
        let val: Option<String> = row.get(0)?;
        Ok(val)
    } else {
        Ok(None)
    }
}

// In crates/thegn-cli/src/commands/pick_agent.rs (inside the sandbox resolution block)
// if let Some(spec) = sandbox::resolve(...) {
//     if let Ok(db) = Db::open() {
//         let _ = db.set_worktree_sandbox(&worktree, spec.backend.binary());
//     }
//     ...
```

**Step 4: Run test to verify pass**

**Step 5: Commit**

```bash
git add crates/thegn-core/src/db.rs crates/thegn-cli/src/commands/pick_agent.rs
git commit -m "feat(sandbox): track resolved sandbox backend in sqlite"
```

---

### Task 2: Sandbox Resource Monitoring API

**Objective:** Add parsing logic for OCI container stats to extract CPU and RAM usage.

**Files:**

- Modify: `crates/thegn-core/src/sandbox.rs`

**Step 1: Write failing parsing test**

```rust
// In crates/thegn-core/src/sandbox.rs tests
#[test]
fn test_parse_sandbox_stats() {
    let output = "1.5%|50MiB / 16GiB";
    let stats = parse_sandbox_stats(output).unwrap();
    assert_eq!(stats.cpu, "1.5%");
    assert_eq!(stats.mem, "50MiB");
}
```

**Step 2: Run test to verify failure**

**Step 3: Write minimal implementation**

```rust
// In crates/thegn-core/src/sandbox.rs

#[derive(Debug, Default, Clone)]
pub struct SandboxStats {
    pub cpu: String,
    pub mem: String,
}

pub fn stats(spec: &SandboxSpec) -> Option<SandboxStats> {
    if !spec.backend.is_oci() {
        return None;
    }
    let rt = spec.backend.binary();
    // format: CPUPerc|MemUsage
    let argv = vec![rt, "stats", "--no-stream", "--format", "{{.CPUPerc}}|{{.MemUsage}}", &spec.name];

    let out = std::process::Command::new(argv[0]).args(&argv[1..]).output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_sandbox_stats(stdout.trim())
}

fn parse_sandbox_stats(output: &str) -> Option<SandboxStats> {
    let parts: Vec<&str> = output.split('|').collect();
    if parts.len() != 2 { return None; }
    let mem = parts[1].split('/').next().unwrap_or(parts[1]).trim().to_string();
    Some(SandboxStats {
        cpu: parts[0].trim().to_string(),
        mem,
    })
}
```

**Step 4: Run test to verify pass**

**Step 5: Commit**

```bash
git add crates/thegn-core/src/sandbox.rs
git commit -m "feat(sandbox): parse cpu and memory utilization from oci runtimes"
```

---

### Task 3: Expose Sandbox Stats to CLI (`stats.rs`)

**Objective:** Append the sandbox stats to the `thegn stats` JSON dump so WASM plugins can fetch it.

**Files:**

- Modify: `crates/thegn-cli/src/commands/stats.rs`

**Step 1: Inspect and Modify `stats.rs`**
_(Since `stats.rs` already exists, inject the `sandbox::stats` call if the active tab is a worktree.)_

```rust
// Pseudo-logic to add inside `stats.rs` (which returns JSON payload to plugins):

use thegn_core::sandbox;

// Look up active worktree...
// if let Some(spec) = sandbox::resolve(...) {
//     if let Some(st) = sandbox::stats(&spec) {
//         // inject "sandbox_cpu": st.cpu, "sandbox_mem": st.mem into JSON
//     }
// }
```

**Step 5: Commit**

```bash
git add crates/thegn-cli/src/commands/stats.rs
git commit -m "feat(sandbox): expose live container stats to wasm plugins via cli"
```

---

### Task 4: Orphan Garbage Collection

**Objective:** Find `thegn-*` containers that lack a backing worktree in SQLite, and `rm -f` them.

**Files:**

- Modify: `crates/thegn-core/src/sandbox.rs`
- Modify: `crates/thegn-cli/src/commands/launch.rs` (Daemon startup)

**Step 1: Write failing GC logic test**

```rust
// In crates/thegn-core/src/sandbox.rs tests
#[test]
fn test_gc_identifies_orphans() {
    let active_wts = vec!["/wt/live".to_string()];
    let containers = vec![
        "thegn-live".to_string(),
        "thegn-dead".to_string(),
        "other-container".to_string()
    ];
    let orphans = identify_orphans(&active_wts, &containers);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0], "thegn-dead");
}
```

**Step 3: Write minimal implementation**

```rust
// In sandbox.rs
pub fn identify_orphans(active_worktrees: &[String], containers: &[String]) -> Vec<String> {
    let active_names: Vec<String> = active_worktrees.iter()
        .map(|w| container_name(w))
        .collect();

    containers.iter()
        .filter(|c| c.starts_with("thegn-"))
        .filter(|c| !active_names.contains(c))
        .cloned()
        .collect()
}

pub fn run_gc(db_worktrees: &[String]) -> Result<(), String> {
    for backend in [Backend::Podman, Backend::Docker] {
        if !crate::util::have(backend.binary()) { continue; }

        let out = std::process::Command::new(backend.binary())
            .args(["ps", "-a", "--format", "{{.Names}}"])
            .output().map_err(|e| e.to_string())?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        let containers: Vec<String> = stdout.lines().map(|s| s.trim().to_string()).collect();

        for orphan in identify_orphans(db_worktrees, &containers) {
            let _ = std::process::Command::new(backend.binary())
                .args(["rm", "-f", &orphan])
                .output();
        }
    }
    Ok(())
}
```

**Step 4: Run test to verify pass**

**Step 5: Hook into Daemon Startup**

```rust
// In crates/thegn-cli/src/commands/launch.rs

// Fire-and-forget thread so we don't block startup
std::thread::spawn(|| {
    if let Ok(db) = Db::open() {
        if let Ok(wts) = db.all_worktrees() { // assuming an accessor exists
             let paths: Vec<String> = wts.into_iter().map(|w| w.path).collect();
             let _ = sandbox::run_gc(&paths);
        }
    }
});
```

**Step 6: Commit**

```bash
git add crates/thegn-core/src/sandbox.rs crates/thegn-cli/src/commands/launch.rs
git commit -m "feat(sandbox): asynchronous orphan garbage collector on daemon launch"
```

---

### Conclusion

This plan ensures all loose ends are tied up: UI visibility (via DB columns), live resource monitoring (via OCI stats scraping), and resilient container lifecycle (via GC).
