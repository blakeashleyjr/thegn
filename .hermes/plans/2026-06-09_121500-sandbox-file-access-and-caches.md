# Sandbox File Access & Advanced Configurations Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Fulfill the complete sandbox requirement matrix by introducing a `file_access` toggle to strictly enforce `all`, `worktree` (default), or `none` file access. Document and demonstrate how existing features already cover `devenv`, `init_script`, build caches, and directory sharing.

**Architecture:**
The previous iterations already implemented `devenv`, `init_script`, `volumes` (for build caches), and `mounts` (for selectively sharing external directories). 
To address "not allowing any file access, or all access", we will introduce `FileAccess` in `SandboxConfig`.
- `FileAccess::Worktree` (default): Mounts the worktree and `.git` common dir. For Bwrap, makes the host `/` read-only (`--ro-bind / /`), and only the worktree read-write.
- `FileAccess::All`: Mounts the host root `/` as read-write to the container `/` (for OCI) and uses `--dev-bind / /` for Bwrap.
- `FileAccess::None`: Mounts no host directories. OCI containers are completely isolated. Bwrap gets a read-only root and no worktree bind.

**Tech Stack:** Rust, Bwrap CLI, Podman/Docker CLI.

---

### Task 1: Add `FileAccess` Enum to Configuration

**Objective:** Add `file_access` to `SandboxConfig` and `SandboxOverlay`.

**Files:**
- Modify: `crates/superzej-core/src/config.rs`

**Step 1: Write failing config parsing test**
```rust
#[test]
fn test_sandbox_file_access_parse() {
    let toml = r#"
        [sandbox]
        file_access = "all"
    "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.sandbox.file_access, FileAccess::All);
}
```

**Step 3: Write minimal implementation**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccess {
    #[default]
    Worktree,
    All,
    None,
}

// Add `pub file_access: FileAccess` to `SandboxConfig`
// Add `pub file_access: Option<FileAccess>` to `SandboxOverlay`
// Update `impl Default` and `apply()` accordingly.
```

**Step 5: Commit**
```bash
git add crates/superzej-core/src/config.rs
git commit -m "feat(sandbox): add file_access enum to config for strict access control"
```

---

### Task 2: Apply `FileAccess` to Sandbox Mounts

**Objective:** Adjust how `sandbox::resolve` populates mounts based on `file_access`.

**Files:**
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 1: Write failing tests for file access**
```rust
#[test]
fn test_file_access_all_mounts_root() {
    // mock config with file_access = All
    // assert mounts contains `Mount { host: "/", dest: "/", ro: false }`
}

#[test]
fn test_file_access_none_mounts_nothing() {
    // mock config with file_access = None
    // assert mounts is empty
}
```

**Step 3: Write minimal implementation**
```rust
// In `sandbox::resolve`:
let mut mounts = vec![];

match cfg.file_access {
    FileAccess::All => {
        mounts.push(Mount { host: "/".into(), dest: "/".into(), ro: false });
    }
    FileAccess::Worktree => {
        mounts.push(Mount { host: loc.path(), dest: loc.path(), ro: false });
        if let Some(gc) = &git_common {
            mounts.push(Mount { host: gc.to_string_lossy().into_owned(), dest: gc.to_string_lossy().into_owned(), ro: false });
        }
    }
    FileAccess::None => {
        // No default worktree or git mounts.
    }
}
// Append user mounts after this.
```

**Step 5: Commit**
```bash
git add crates/superzej-core/src/sandbox.rs
git commit -m "feat(sandbox): control volume and worktree mounts via file_access level"
```

---

### Task 3: Adjust Runtime Argv for File Access

**Objective:** Modify `sandbox::enter_argv` to respect `FileAccess` (avoiding `--workdir` when `None`, enforcing `--ro-bind` for bwrap).

**Files:**
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 3: Write minimal implementation**
```rust
// Inside `backend_enter_argv` for Podman/Docker:
let mut v = vec![rt.to_string(), "exec".into(), "-it".into()];
if spec.file_access != FileAccess::None {
    v.extend(["--workdir".into(), wt.clone()]);
}

// Inside `backend_enter_argv` for Bwrap:
let mut v = vec!["bwrap".to_string()];
if spec.file_access == FileAccess::All {
    v.extend(["--dev-bind".into(), "/".into(), "/".into()]);
} else {
    v.extend(["--ro-bind".into(), "/".into(), "/".into()]);
    v.extend(["--dev-bind".into(), "/dev".into(), "/dev".into()]);
}

if spec.file_access != FileAccess::None {
    v.extend(["--chdir".into(), wt.clone()]);
}
```

**Step 5: Commit**
```bash
git add crates/superzej-core/src/sandbox.rs
git commit -m "feat(sandbox): enforce read-only hosts and restrict workdirs for file_access none/worktree"
```